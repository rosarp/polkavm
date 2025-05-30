#![allow(clippy::exit)]
#![allow(clippy::print_stdout)]
#![allow(clippy::print_stderr)]
#![allow(clippy::use_debug)]

use clap::Parser;
use core::fmt::Write;
use polkavm::{Engine, InterruptKind, Module, ModuleConfig, ProgramBlob, Reg};
use polkavm_common::assembler::assemble;
use polkavm_common::program::{asm, ProgramCounter, ProgramParts, ISA64_V1};
use std::path::{Path, PathBuf};

#[derive(Parser, Debug)]
#[clap(version)]
enum Args {
    Generate,
    Test,
}

fn main() {
    env_logger::init();

    let args = Args::parse();
    match args {
        Args::Generate => main_generate(),
        Args::Test => main_test(),
    }
}

struct Testcase {
    disassembly: String,
    json: TestcaseJson,
}

#[derive(serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "kebab-case")]
struct Page {
    address: u32,
    length: u32,
    is_writable: bool,
}

#[derive(PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "kebab-case")]
struct MemoryChunk {
    address: u32,
    contents: Vec<u8>,
}

#[derive(serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "kebab-case")]
struct TestcaseJson {
    name: String,
    initial_regs: [u64; 13],
    initial_pc: u32,
    initial_page_map: Vec<Page>,
    initial_memory: Vec<MemoryChunk>,
    initial_gas: i64,
    program: Vec<u8>,
    expected_status: String,
    expected_regs: Vec<u64>,
    expected_pc: u32,
    expected_memory: Vec<MemoryChunk>,
    expected_gas: i64,
    #[serde(skip_serializing_if = "Option::is_none")]
    expected_page_fault_address: Option<u32>,
}

fn extract_chunks(base_address: u32, slice: &[u8]) -> Vec<MemoryChunk> {
    let mut output = Vec::new();
    let mut position = 0;
    while let Some(next_position) = slice[position..].iter().position(|&byte| byte != 0).map(|offset| position + offset) {
        position = next_position;
        let length = slice[position..].iter().take_while(|&&byte| byte != 0).count();
        output.push(MemoryChunk {
            address: base_address + position as u32,
            contents: slice[position..position + length].into(),
        });
        position += length;
    }

    output
}

enum ProgramCounterRef {
    ByLabel { label: String, instruction_offset: u32 },
    Preset(ProgramCounter),
}

#[derive(Default)]
struct PrePost {
    gas: Option<i64>,
    regs: [Option<u64>; 13],
    pc: Option<ProgramCounterRef>,
}

fn parse_pre_post(line: &str, output: &mut PrePost) {
    let line = line.trim();
    let index = line.find('=').expect("invalid 'pre' / 'post' directive: no '=' found");
    let lhs = line[..index].trim();
    let rhs = line[index + 1..].trim();
    if lhs == "gas" {
        output.gas = Some(rhs.parse::<i64>().expect("invalid 'pre' / 'post' directive: failed to parse rhs"));
    } else if lhs == "pc" {
        let rhs = rhs
            .strip_prefix('@')
            .expect("invalid 'pre' / 'post' directive: failed to parse 'pc': no '@' found")
            .trim();
        let index = rhs
            .find('[')
            .expect("invalid 'pre' / 'post' directive: failed to parse 'pc': no '[' found");
        let label = &rhs[..index];
        let rhs = &rhs[index + 1..];
        let index = rhs
            .find(']')
            .expect("invalid 'pre' / 'post' directive: failed to parse 'pc': no ']' found");
        let offset = rhs[..index]
            .parse::<u32>()
            .expect("invalid 'pre' / 'post' directive: failed to parse 'pc': invalid offset");
        if !rhs[index + 1..].trim().is_empty() {
            panic!("invalid 'pre' / 'post' directive: failed to parse 'pc': junk after ']'");
        }

        output.pc = Some(ProgramCounterRef::ByLabel {
            label: label.to_owned(),
            instruction_offset: offset,
        });
    } else {
        let lhs = polkavm_common::utils::parse_reg(lhs).expect("invalid 'pre' / 'post' directive: failed to parse lhs");
        let rhs = polkavm_common::utils::parse_immediate(rhs)
            .map(Into::into)
            .expect("invalid 'pre' / 'post' directive: failed to parse rhs");
        output.regs[lhs as usize] = Some(rhs);
    }
}

fn main_generate() {
    let mut tests = Vec::new();

    let mut config = polkavm::Config::new();
    config.set_backend(Some(polkavm::BackendKind::Interpreter));
    config.set_allow_dynamic_paging(true);

    let engine = Engine::new(&config).unwrap();
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("spec");
    let mut found_errors = false;

    let mut paths: Vec<PathBuf> = std::fs::read_dir(root.join("src"))
        .unwrap()
        .map(|entry| entry.unwrap().path())
        .collect();

    paths.sort_by_key(|entry| entry.file_stem().unwrap().to_string_lossy().to_string());

    struct RawTestcase {
        name: String,
        internal_name: String,
        blob: Vec<u8>,
        pre: PrePost,
        post: PrePost,
        expected_status: Option<&'static str>,
    }

    let mut testcases = Vec::new();
    for path in paths {
        let name = path.file_stem().unwrap().to_string_lossy();

        let mut pre = PrePost::default();
        let mut post = PrePost::default();

        let input = std::fs::read_to_string(&path).unwrap();
        let mut input_lines = Vec::new();
        for line in input.lines() {
            if let Some(line) = line.strip_prefix("pre:") {
                parse_pre_post(line, &mut pre);
                input_lines.push(""); // Insert dummy line to not mess up the line count.
                continue;
            }

            if let Some(line) = line.strip_prefix("post:") {
                parse_pre_post(line, &mut post);
                input_lines.push(""); // Insert dummy line to not mess up the line count.
                continue;
            }

            input_lines.push(line);
        }

        let input = input_lines.join("\n");
        let blob = match assemble(&input) {
            Ok(blob) => blob,
            Err(error) => {
                eprintln!("Failed to assemble {path:?}: {error}");
                found_errors = true;
                continue;
            }
        };

        testcases.push(RawTestcase {
            name: name.into_owned(),
            internal_name: format!("{path:?}"),
            blob,
            pre,
            post,
            expected_status: None,
        });
    }

    // This is kind of a hack, but whatever.
    for line in include_str!("../../../crates/polkavm/src/tests_riscv.rs").lines() {
        let prefix = "riscv_test!(riscv_unoptimized_rv64";
        if !line.starts_with(prefix) {
            continue;
        }

        let line = &line[prefix.len()..];
        let mut xs = line.split(',');
        let name = xs.next().unwrap();
        let path = xs.next().unwrap().trim();
        let path = &path[1..path.len() - 1];

        let path = root.join("../../../crates/polkavm/src").join(path);
        let path = path.canonicalize().unwrap();
        let elf = std::fs::read(&path).unwrap();

        let mut linker_config = polkavm_linker::Config::default();
        linker_config.set_opt_level(polkavm_linker::OptLevel::O1);
        linker_config.set_strip(true);
        linker_config.set_min_stack_size(0);
        let blob = polkavm_linker::program_from_elf(linker_config, &elf).unwrap();

        let mut post = PrePost::default();

        let program_blob = ProgramBlob::parse(blob.clone().into()).unwrap();
        post.pc = Some(ProgramCounterRef::Preset(
            program_blob
                .instructions(ISA64_V1)
                .find(|inst| inst.kind == asm::ret())
                .unwrap()
                .offset,
        ));

        testcases.push(RawTestcase {
            name: format!("riscv_rv64{name}"),
            internal_name: format!("{path:?}"),
            blob,
            pre: PrePost::default(),
            post,
            expected_status: Some("halt"),
        });
    }

    for testcase in testcases {
        let RawTestcase {
            name,
            internal_name,
            blob,
            pre,
            post,
            expected_status,
        } = testcase;

        let initial_gas = pre.gas.unwrap_or(10000);
        let initial_regs = pre.regs.map(|value| value.unwrap_or(0));
        assert!(pre.pc.is_none(), "'pre: pc = ...' is currently unsupported");

        let parts = ProgramParts::from_bytes(blob.into()).unwrap();
        let blob = ProgramBlob::from_parts(parts.clone()).unwrap();

        let mut module_config = ModuleConfig::default();
        module_config.set_strict(true);
        module_config.set_gas_metering(Some(polkavm::GasMeteringKind::Sync));
        module_config.set_step_tracing(true);
        module_config.set_dynamic_paging(true);

        let module = Module::from_blob(&engine, &module_config, blob.clone()).unwrap();
        let mut instance = module.instantiate().unwrap();

        let mut initial_page_map = Vec::new();
        let mut initial_memory = Vec::new();

        if module.memory_map().ro_data_size() > 0 {
            initial_page_map.push(Page {
                address: module.memory_map().ro_data_address(),
                length: module.memory_map().ro_data_size(),
                is_writable: false,
            });

            initial_memory.extend(extract_chunks(module.memory_map().ro_data_address(), blob.ro_data()));
        }

        if module.memory_map().rw_data_size() > 0 {
            initial_page_map.push(Page {
                address: module.memory_map().rw_data_address(),
                length: module.memory_map().rw_data_size(),
                is_writable: true,
            });

            initial_memory.extend(extract_chunks(module.memory_map().rw_data_address(), blob.rw_data()));
        }

        if module.memory_map().stack_size() > 0 {
            initial_page_map.push(Page {
                address: module.memory_map().stack_address_low(),
                length: module.memory_map().stack_size(),
                is_writable: true,
            });
        }

        let initial_pc = blob.exports().find(|export| export.symbol() == "main").unwrap().program_counter();

        let expected_final_pc = if let Some(export) = blob.exports().find(|export| export.symbol() == "expected_exit") {
            assert!(
                post.pc.is_none(),
                "'@expected_exit' label and 'post: pc = ...' should not be used together"
            );
            export.program_counter().0
        } else if let Some(ProgramCounterRef::ByLabel { label, instruction_offset }) = post.pc {
            let Some(export) = blob.exports().find(|export| export.symbol().as_bytes() == label.as_bytes()) else {
                panic!("label specified in 'post: pc = ...' is missing: @{label}");
            };

            let instructions: Vec<_> = blob.instructions(ISA64_V1).collect();
            let index = instructions
                .iter()
                .position(|inst| inst.offset == export.program_counter())
                .expect("failed to find label specified in 'post: pc = ...'");
            let instruction = instructions
                .get(index + instruction_offset as usize)
                .expect("invalid 'post: pc = ...': offset goes out of bounds of the basic block");
            instruction.offset.0
        } else if let Some(ProgramCounterRef::Preset(pc)) = post.pc {
            pc.0
        } else {
            blob.code().len() as u32
        };

        instance.set_gas(initial_gas);
        instance.set_next_program_counter(initial_pc);

        for (reg, value) in Reg::ALL.into_iter().zip(initial_regs) {
            instance.set_reg(reg, value);
        }

        if module_config.dynamic_paging() {
            for page in &initial_page_map {
                instance.zero_memory(page.address, page.length).unwrap();
                if !page.is_writable {
                    instance.protect_memory(page.address, page.length).unwrap();
                }
            }

            for chunk in &initial_memory {
                instance.write_memory(chunk.address, &chunk.contents).unwrap();
            }
        }

        let mut final_pc = initial_pc;
        let (final_status, page_fault_address) = loop {
            match instance.run().unwrap() {
                InterruptKind::Finished => break ("halt", None),
                InterruptKind::Trap => break ("panic", None),
                InterruptKind::Ecalli(..) => todo!(),
                InterruptKind::NotEnoughGas => break ("out-of-gas", None),
                InterruptKind::Segfault(segfault) => break ("page-fault", Some(segfault.page_address)),
                InterruptKind::Step => {
                    final_pc = instance.program_counter().unwrap();
                    continue;
                }
            }
        };

        if final_status != "halt" {
            final_pc = instance.program_counter().unwrap();
        }

        if let Some(expected_status) = expected_status {
            if final_status != expected_status {
                eprintln!("Unexpected final status for {internal_name}: expected {expected_status}, is {final_status}");
                found_errors = true;
                continue;
            }
        }

        if final_pc.0 != expected_final_pc {
            eprintln!("Unexpected final program counter for {internal_name}: expected {expected_final_pc}, is {final_pc}");
            found_errors = true;
            continue;
        }

        let mut expected_regs = Vec::new();
        for reg in Reg::ALL {
            let value = instance.reg(reg);
            expected_regs.push(value);
        }

        let mut expected_memory = Vec::new();
        for page in &initial_page_map {
            let memory = instance.read_memory(page.address, page.length).unwrap();
            expected_memory.extend(extract_chunks(page.address, &memory));
        }

        let expected_gas = instance.gas();

        let mut found_post_check_errors = false;

        for ((final_value, reg), required_value) in expected_regs.iter().zip(Reg::ALL).zip(post.regs.iter()) {
            if let Some(required_value) = required_value {
                if final_value != required_value {
                    eprintln!("{internal_name}: unexpected {reg}: 0x{final_value:x} (expected: 0x{required_value:x})");
                    found_post_check_errors = true;
                }
            }
        }

        if let Some(post_gas) = post.gas {
            if expected_gas != post_gas {
                eprintln!("{internal_name}: unexpected gas: {expected_gas} (expected: {post_gas})");
                found_post_check_errors = true;
            }
        }

        if found_post_check_errors {
            found_errors = true;
            continue;
        }

        let mut disassembler = polkavm_disassembler::Disassembler::new(&blob, polkavm_disassembler::DisassemblyFormat::Guest).unwrap();
        disassembler.show_raw_bytes(true);
        disassembler.prefer_non_abi_reg_names(true);
        disassembler.prefer_unaliased(true);
        disassembler.prefer_offset_jump_targets(true);
        disassembler.emit_header(false);
        disassembler.emit_exports(false);

        let mut disassembly = Vec::new();
        disassembler.disassemble_into(&mut disassembly).unwrap();
        let disassembly = String::from_utf8(disassembly).unwrap();

        tests.push(Testcase {
            disassembly,
            json: TestcaseJson {
                name,
                initial_regs,
                initial_pc: initial_pc.0,
                initial_page_map,
                initial_memory,
                initial_gas,
                program: parts.code_and_jump_table.to_vec(),
                expected_status: final_status.to_owned(),
                expected_regs,
                expected_pc: expected_final_pc,
                expected_memory,
                expected_gas,
                expected_page_fault_address: page_fault_address,
            },
        });
    }

    tests.sort_by_key(|test| test.json.name.clone());

    let output_programs_root = root.join("output").join("programs");
    std::fs::create_dir_all(&output_programs_root).unwrap();

    let mut index_md = String::new();
    writeln!(&mut index_md, "# Testcases\n").unwrap();
    writeln!(&mut index_md, "This file contains a human-readable index of all of the testcases,").unwrap();
    writeln!(&mut index_md, "along with their disassemblies and other relevant information.\n\n").unwrap();

    for test in tests {
        let payload = serde_json::to_string_pretty(&test.json).unwrap();
        let output_path = output_programs_root.join(format!("{}.json", test.json.name));
        if !std::fs::read(&output_path)
            .map(|old_payload| old_payload == payload.as_bytes())
            .unwrap_or(false)
        {
            println!("Generating {output_path:?}...");
            std::fs::write(output_path, payload).unwrap();
        }

        writeln!(&mut index_md, "## {}\n", test.json.name).unwrap();

        if !test.json.initial_page_map.is_empty() {
            writeln!(&mut index_md, "Initial page map:").unwrap();
            for page in &test.json.initial_page_map {
                let access = if page.is_writable { "RW" } else { "RO" };

                writeln!(
                    &mut index_md,
                    "   * {access}: 0x{:x}-0x{:x} (0x{:x} bytes)",
                    page.address,
                    page.address + page.length,
                    page.length
                )
                .unwrap();
            }

            writeln!(&mut index_md).unwrap();
        }

        if !test.json.initial_memory.is_empty() {
            writeln!(&mut index_md, "Initial non-zero memory chunks:").unwrap();
            for chunk in &test.json.initial_memory {
                let contents: Vec<_> = chunk.contents.iter().map(|byte| format!("0x{:02x}", byte)).collect();
                let contents = contents.join(", ");
                writeln!(
                    &mut index_md,
                    "   * 0x{:x}-0x{:x} (0x{:x} bytes) = [{}]",
                    chunk.address,
                    chunk.address + chunk.contents.len() as u32,
                    chunk.contents.len(),
                    contents
                )
                .unwrap();
            }

            writeln!(&mut index_md).unwrap();
        }

        if test.json.initial_regs.iter().any(|value| *value != 0) {
            writeln!(&mut index_md, "Initial non-zero registers:").unwrap();
            for reg in Reg::ALL {
                let value = test.json.initial_regs[reg as usize];
                if value != 0 {
                    writeln!(&mut index_md, "   * {} = 0x{:x}", reg.name_non_abi(), value).unwrap();
                }
            }

            writeln!(&mut index_md).unwrap();
        }

        writeln!(&mut index_md, "```\n{}```\n", test.disassembly).unwrap();

        if test
            .json
            .initial_regs
            .iter()
            .zip(test.json.expected_regs.iter())
            .any(|(old_value, new_value)| *old_value != *new_value)
        {
            writeln!(&mut index_md, "Registers after execution (only changed registers):").unwrap();
            for reg in Reg::ALL {
                let value_before = test.json.initial_regs[reg as usize];
                let value_after = test.json.expected_regs[reg as usize];
                if value_before != value_after {
                    writeln!(
                        &mut index_md,
                        "   * {} = 0x{:x} (initially was 0x{:x})",
                        reg.name_non_abi(),
                        value_after,
                        value_before
                    )
                    .unwrap();
                }
            }

            writeln!(&mut index_md).unwrap();
        }

        if !test.json.expected_memory.is_empty() {
            if test.json.expected_memory == test.json.initial_memory {
                writeln!(&mut index_md, "The memory contents after execution should be unchanged.").unwrap();
            } else {
                writeln!(&mut index_md, "Final non-zero memory chunks:").unwrap();
                for chunk in &test.json.expected_memory {
                    let contents: Vec<_> = chunk.contents.iter().map(|byte| format!("0x{:02x}", byte)).collect();
                    let contents = contents.join(", ");
                    writeln!(
                        &mut index_md,
                        "   * 0x{:x}-0x{:x} (0x{:x} bytes) = [{}]",
                        chunk.address,
                        chunk.address + chunk.contents.len() as u32,
                        chunk.contents.len(),
                        contents
                    )
                    .unwrap();
                }
            }

            writeln!(&mut index_md).unwrap();
        }

        assert_eq!(
            test.json.expected_status == "page-fault",
            test.json.expected_page_fault_address.is_some()
        );
        write!(&mut index_md, "Program should end with: {}", test.json.expected_status).unwrap();

        if let Some(address) = test.json.expected_page_fault_address {
            write!(&mut index_md, " (page address = 0x{:x})", address).unwrap();
        }

        writeln!(&mut index_md, "\n").unwrap();
        writeln!(&mut index_md, "Final value of the program counter: {}\n", test.json.expected_pc).unwrap();
        writeln!(
            &mut index_md,
            "Gas consumed: {} -> {}\n",
            test.json.initial_gas, test.json.expected_gas
        )
        .unwrap();
        writeln!(&mut index_md).unwrap();
    }

    std::fs::write(root.join("output").join("TESTCASES.md"), index_md).unwrap();

    if found_errors {
        std::process::exit(1);
    }
}

fn main_test() {
    todo!();
}
