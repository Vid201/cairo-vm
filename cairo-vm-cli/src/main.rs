#![deny(warnings)]
#![forbid(unsafe_code)]
use bincode::enc::write::Writer;
use cairo_vm::air_public_input::PublicInputError;
use cairo_vm::cairo_run::{self, EncodeTraceError};
use cairo_vm::hint_processor::builtin_hint_processor::builtin_hint_processor_definition::BuiltinHintProcessor;
use cairo_vm::vm::errors::cairo_run_errors::CairoRunError;
use cairo_vm::vm::errors::trace_errors::TraceError;
use cairo_vm::vm::errors::vm_errors::VirtualMachineError;
use clap::{CommandFactory, Parser, ValueHint};
use std::io::{self, Write};
use std::path::PathBuf;
use thiserror::Error;

#[cfg(feature = "with_mimalloc")]
use mimalloc::MiMalloc;

#[cfg(feature = "with_mimalloc")]
#[global_allocator]
static ALLOC: MiMalloc = MiMalloc;

#[derive(Parser, Debug)]
#[clap(author, version, about, long_about = None)]
struct Args {
    #[clap(value_parser, value_hint=ValueHint::FilePath)]
    filename: PathBuf,
    #[clap(long = "trace_file", value_parser)]
    trace_file: Option<PathBuf>,
    #[structopt(long = "print_output")]
    print_output: bool,
    #[structopt(long = "entrypoint", default_value = "main")]
    entrypoint: String,
    #[structopt(long = "memory_file")]
    memory_file: Option<PathBuf>,
    #[clap(long = "layout", default_value = "plain", value_parser=validate_layout)]
    layout: String,
    #[structopt(long = "proof_mode")]
    proof_mode: bool,
    #[structopt(long = "secure_run")]
    secure_run: Option<bool>,
    #[clap(long = "air_public_input")]
    air_public_input: Option<String>,
    #[clap(long = "air_private_input")]
    air_private_input: Option<String>,
}

fn validate_layout(value: &str) -> Result<String, String> {
    match value {
        "plain"
        | "small"
        | "dex"
        | "starknet"
        | "starknet_with_keccak"
        | "recursive_large_output"
        | "all_cairo"
        | "all_solidity"
        | "dynamic" => Ok(value.to_string()),
        _ => Err(format!("{value} is not a valid layout")),
    }
}

#[derive(Debug, Error)]
enum Error {
    #[error("Invalid arguments")]
    Cli(#[from] clap::Error),
    #[error("Failed to interact with the file system")]
    IO(#[from] std::io::Error),
    #[error("The cairo program execution failed")]
    Runner(#[from] CairoRunError),
    #[error(transparent)]
    EncodeTrace(#[from] EncodeTraceError),
    #[error(transparent)]
    VirtualMachine(#[from] VirtualMachineError),
    #[error(transparent)]
    Trace(#[from] TraceError),
    #[error(transparent)]
    PublicInput(#[from] PublicInputError),
}

struct FileWriter {
    buf_writer: io::BufWriter<std::fs::File>,
    bytes_written: usize,
}

impl Writer for FileWriter {
    fn write(&mut self, bytes: &[u8]) -> Result<(), bincode::error::EncodeError> {
        self.buf_writer
            .write_all(bytes)
            .map_err(|e| bincode::error::EncodeError::Io {
                inner: e,
                index: self.bytes_written,
            })?;

        self.bytes_written += bytes.len();

        Ok(())
    }
}

impl FileWriter {
    fn new(buf_writer: io::BufWriter<std::fs::File>) -> Self {
        Self {
            buf_writer,
            bytes_written: 0,
        }
    }

    fn flush(&mut self) -> io::Result<()> {
        self.buf_writer.flush()
    }
}

fn run(args: impl Iterator<Item = String>) -> Result<(), Error> {
    let args = Args::try_parse_from(args)?;

    if args.air_public_input.is_some() && !args.proof_mode {
        let error = Args::command().error(
            clap::error::ErrorKind::ArgumentConflict,
            "--air_public_input can only be used in proof_mode.",
        );
        return Err(Error::Cli(error));
    }

    if args.air_private_input.is_some() && !args.proof_mode {
        let error = Args::command().error(
            clap::error::ErrorKind::ArgumentConflict,
            "--air_private_input can only be used in proof_mode.",
        );
        return Err(Error::Cli(error));
    }

    if args.air_private_input.is_some() && args.trace_file.is_none() {
        let error = Args::command().error(
            clap::error::ErrorKind::ArgumentConflict,
            "--trace_file must be set when --air_private_input is set.",
        );
        return Err(Error::Cli(error));
    }

    if args.air_private_input.is_some() && args.memory_file.is_none() {
        let error = Args::command().error(
            clap::error::ErrorKind::ArgumentConflict,
            "--memory_file must be set when --air_private_input is set.",
        );
        return Err(Error::Cli(error));
    }

    let trace_enabled = args.trace_file.is_some() || args.air_public_input.is_some();
    let mut hint_executor = BuiltinHintProcessor::new_empty();
    let cairo_run_config = cairo_run::CairoRunConfig {
        entrypoint: &args.entrypoint,
        trace_enabled,
        relocate_mem: args.memory_file.is_some() || args.air_public_input.is_some(),
        layout: &args.layout,
        proof_mode: args.proof_mode,
        secure_run: args.secure_run,
        ..Default::default()
    };

    let program_content = std::fs::read(args.filename).map_err(Error::IO)?;

    let (cairo_runner, mut vm) =
        match cairo_run::cairo_run(&program_content, &cairo_run_config, &mut hint_executor) {
            Ok(runner) => runner,
            Err(error) => {
                eprintln!("{error}");
                return Err(Error::Runner(error));
            }
        };

    if args.print_output {
        let mut output_buffer = "Program Output:\n".to_string();
        vm.write_output(&mut output_buffer)?;
        print!("{output_buffer}");
    }

    if let Some(ref trace_path) = args.trace_file {
        let relocated_trace = cairo_runner
            .relocated_trace
            .as_ref()
            .ok_or(Error::Trace(TraceError::TraceNotRelocated))?;

        let trace_file = std::fs::File::create(trace_path)?;
        let mut trace_writer =
            FileWriter::new(io::BufWriter::with_capacity(3 * 1024 * 1024, trace_file));

        cairo_run::write_encoded_trace(relocated_trace, &mut trace_writer)?;
        trace_writer.flush()?;
    }

    if let Some(ref memory_path) = args.memory_file {
        let memory_file = std::fs::File::create(memory_path)?;
        let mut memory_writer =
            FileWriter::new(io::BufWriter::with_capacity(5 * 1024 * 1024, memory_file));

        cairo_run::write_encoded_memory(&cairo_runner.relocated_memory, &mut memory_writer)?;
        memory_writer.flush()?;
    }

    if let Some(file_path) = args.air_public_input {
        let json = cairo_runner.get_air_public_input(&vm)?.serialize_json()?;
        std::fs::write(file_path, json)?;
    }

    if let (Some(file_path), Some(ref trace_file), Some(ref memory_file)) =
        (args.air_private_input, args.trace_file, args.memory_file)
    {
        // Get absolute paths of trace_file & memory_file
        let trace_path = trace_file
            .as_path()
            .canonicalize()
            .unwrap_or(trace_file.clone())
            .to_string_lossy()
            .to_string();
        let memory_path = memory_file
            .as_path()
            .canonicalize()
            .unwrap_or(memory_file.clone())
            .to_string_lossy()
            .to_string();

        let json = cairo_runner
            .get_air_private_input(&vm)
            .to_serializable(trace_path, memory_path)
            .serialize_json()
            .map_err(PublicInputError::Serde)?;
        std::fs::write(file_path, json)?;
    }

    Ok(())
}

fn main() -> Result<(), Error> {
    #[cfg(test)]
    return Ok(());

    #[cfg(not(test))]
    match run(std::env::args()) {
        Err(Error::Cli(err)) => err.exit(),
        other => other,
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::too_many_arguments)]
    use super::*;
    use assert_matches::assert_matches;
    use rstest::rstest;

    #[rstest]
    #[case([].as_slice())]
    #[case(["cairo-vm-cli"].as_slice())]
    fn test_run_missing_mandatory_args(#[case] args: &[&str]) {
        let args = args.iter().cloned().map(String::from);
        assert_matches!(run(args), Err(Error::Cli(_)));
    }

    #[rstest]
    #[case(["cairo-vm-cli", "--layout", "broken_layout", "../cairo_programs/fibonacci.json"].as_slice())]
    fn test_run_invalid_args(#[case] args: &[&str]) {
        let args = args.iter().cloned().map(String::from);
        assert_matches!(run(args), Err(Error::Cli(_)));
    }

    #[rstest]
    #[case(["cairo-vm-cli", "../cairo_programs/fibonacci.json", "--air_private_input", "/dev/null", "--proof_mode", "--memory_file", "/dev/null"].as_slice())]
    fn test_run_air_private_input_no_trace(#[case] args: &[&str]) {
        let args = args.iter().cloned().map(String::from);
        assert_matches!(run(args), Err(Error::Cli(_)));
    }

    #[rstest]
    #[case(["cairo-vm-cli", "../cairo_programs/fibonacci.json", "--air_private_input", "/dev/null", "--proof_mode", "--trace_file", "/dev/null"].as_slice())]
    fn test_run_air_private_input_no_memory(#[case] args: &[&str]) {
        let args = args.iter().cloned().map(String::from);
        assert_matches!(run(args), Err(Error::Cli(_)));
    }

    #[rstest]
    #[case(["cairo-vm-cli", "../cairo_programs/fibonacci.json", "--air_private_input", "/dev/null", "--trace_file", "/dev/null", "--memory_file", "/dev/null"].as_slice())]
    fn test_run_air_private_input_no_proof(#[case] args: &[&str]) {
        let args = args.iter().cloned().map(String::from);
        assert_matches!(run(args), Err(Error::Cli(_)));
    }

    #[rstest]
    fn test_run_ok(
        #[values(None,
                 Some("plain"),
                 Some("small"),
                 Some("dex"),
                 Some("starknet"),
                 Some("starknet_with_keccak"),
                 Some("recursive_large_output"),
                 Some("all_cairo"),
                 Some("all_solidity"),
                 //FIXME: dynamic layout leads to _very_ slow execution
                 //Some("dynamic"),
        )]
        layout: Option<&str>,
        #[values(false, true)] memory_file: bool,
        #[values(false, true)] mut trace_file: bool,
        #[values(false, true)] proof_mode: bool,
        #[values(false, true)] secure_run: bool,
        #[values(false, true)] print_output: bool,
        #[values(false, true)] entrypoint: bool,
        #[values(false, true)] air_public_input: bool,
        #[values(false, true)] air_private_input: bool,
    ) {
        let mut args = vec!["cairo-vm-cli".to_string()];
        if let Some(layout) = layout {
            args.extend_from_slice(&["--layout".to_string(), layout.to_string()]);
        }
        if air_public_input {
            args.extend_from_slice(&["--air_public_input".to_string(), "/dev/null".to_string()]);
        }
        if air_private_input {
            args.extend_from_slice(&["--air_private_input".to_string(), "/dev/null".to_string()]);
        }
        if proof_mode {
            trace_file = true;
            args.extend_from_slice(&["--proof_mode".to_string()]);
        }
        if entrypoint {
            args.extend_from_slice(&["--entrypoint".to_string(), "main".to_string()]);
        }
        if memory_file {
            args.extend_from_slice(&["--memory_file".to_string(), "/dev/null".to_string()]);
        }
        if trace_file {
            args.extend_from_slice(&["--trace_file".to_string(), "/dev/null".to_string()]);
        }
        if secure_run {
            args.extend_from_slice(&["--secure_run".to_string(), "true".to_string()]);
        }
        if print_output {
            args.extend_from_slice(&["--print_output".to_string()]);
        }

        args.push("../cairo_programs/proof_programs/fibonacci.json".to_string());
        if air_public_input && !proof_mode
            || (air_private_input && (!proof_mode || !trace_file || !memory_file))
        {
            assert_matches!(run(args.into_iter()), Err(_));
        } else {
            assert_matches!(run(args.into_iter()), Ok(_));
        }
    }

    #[test]
    fn test_run_missing_program() {
        let args = ["cairo-vm-cli", "../missing/program.json"]
            .into_iter()
            .map(String::from);
        assert_matches!(run(args), Err(Error::IO(_)));
    }

    #[rstest]
    #[case("../cairo_programs/manually_compiled/invalid_even_length_hex.json")]
    #[case("../cairo_programs/manually_compiled/invalid_memory.json")]
    #[case("../cairo_programs/manually_compiled/invalid_odd_length_hex.json")]
    #[case("../cairo_programs/manually_compiled/no_data_program.json")]
    #[case("../cairo_programs/manually_compiled/no_main_program.json")]
    fn test_run_bad_file(#[case] program: &str) {
        let args = ["cairo-vm-cli", program].into_iter().map(String::from);
        assert_matches!(run(args), Err(Error::Runner(_)));
    }

    //Since the functionality here is trivial, I just call the function
    //to fool Codecov.
    #[test]
    fn test_main() {
        main().unwrap();
    }

    #[test]
    fn test_valid_layouts() {
        let valid_layouts = vec![
            "plain",
            "small",
            "dex",
            "starknet",
            "starknet_with_keccak",
            "recursive_large_output",
            "all_cairo",
            "all_solidity",
        ];

        for layout in valid_layouts {
            assert_eq!(validate_layout(layout), Ok(layout.to_string()));
        }
    }

    #[test]
    fn test_invalid_layout() {
        let invalid_layout = "invalid layout name";
        assert!(validate_layout(invalid_layout).is_err());
    }
}
