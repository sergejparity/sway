use crate::{
    error::AdapterError,
    types::{Breakpoints, Instruction},
};
use dap::types::StartDebuggingRequestKind;
use forc_pkg::BuiltPackage;
use forc_test::{execute::TestExecutor, setup::TestSetup, TestResult};
use sway_core::source_map::SourceMap;
use std::path::PathBuf;

#[derive(Default, Debug, Clone)]
/// The state of the DAP server.
pub struct ServerState {
    // DAP state
    pub program_path: PathBuf,
    pub mode: Option<StartDebuggingRequestKind>,
    pub initialized_event_sent: bool,
    pub started_debugging: bool,
    pub configuration_done: bool,
    pub breakpoints_need_update: bool,
    pub stopped_on_breakpoint_id: Option<i64>,
    pub breakpoints: Breakpoints,

    // Build state
    pub source_map: SourceMap,
    pub built_package: Option<BuiltPackage>,

    // Test state
    pub test_setup: Option<TestSetup>,
    pub test_results: Vec<forc_test::TestResult>,
    pub executors: Vec<TestExecutor>,
    original_executors: Vec<TestExecutor>,
}

impl ServerState {
    /// Resets the data for a new run of the tests.
    pub fn reset(&mut self) {
        self.started_debugging = false;
        self.executors.clone_from(&self.original_executors);
        self.built_package = None;
        self.test_setup = None;
        self.test_results = vec![];
        self.stopped_on_breakpoint_id = None;
        self.breakpoints_need_update = true;
    }

    /// Initializes the executor stores.
    pub fn init_executors(&mut self, executors: Vec<TestExecutor>) {
        self.executors.clone_from(&executors);
        self.original_executors = executors;
    }

    /// Returns the active [TestExecutor], if any.
    pub fn executor(&mut self) -> Option<&mut TestExecutor> {
        self.executors.first_mut()
    }

    /// Finds the source location matching a VM program counter.
    pub fn vm_pc_to_source_location(
        &self,
        pc: Instruction,
    ) -> Result<(&PathBuf, i64), AdapterError> {
        // Convert instruction to byte offset (pc/4 for word addressing)
        if let Some((path, location)) = self.source_map.addr_to_span(pc as usize / 4) {
            Ok((&path, location.start.line as i64))
        } else {
            Err(AdapterError::MissingSourceMap { pc })
        }
    }
    // pub fn vm_pc_to_source_location(
    //     &self,
    //     pc: Instruction,
    // ) -> Result<(&PathBuf, i64), AdapterError> {
    //     // Try to find the source location by looking forupdate_vm_breakpoints the program counter in the source map.
    //     self.source_map
    //         .iter()
    //         .find_map(|(source_path, source_map)| {
    //             for (&line, instructions) in source_map {
    //                 // Divide by 4 to get the opcode offset rather than the program counter offset.
    //                 let instruction_offset = pc / 4;
    //                 if instructions
    //                     .iter()
    //                     .any(|instruction| instruction_offset == *instruction)
    //                 {
    //                     return Some((source_path, line));
    //                 }
    //             }
    //             None
    //         })
    //         .ok_or(AdapterError::MissingSourceMap { pc })
    // }

    /// Finds the breakpoint matching a VM program counter.
    pub fn vm_pc_to_breakpoint_id(&self, pc: u64) -> Result<i64, AdapterError> {
        let (source_path, source_line) = self.vm_pc_to_source_location(pc)?;

        // Find the breakpoint ID matching the source location.
        let source_bps = self
            .breakpoints
            .get(source_path)
            .ok_or(AdapterError::UnknownBreakpoint { pc })?;
        let breakpoint_id = source_bps
            .iter()
            .find_map(|bp| {
                if bp.line == Some(source_line) {
                    bp.id
                } else {
                    None
                }
            })
            .ok_or(AdapterError::UnknownBreakpoint { pc })?;

        Ok(breakpoint_id)
    }

    /// Updates the breakpoints in the VM for all remaining [TestExecutor]s.
    pub(crate) fn update_vm_breakpoints(&mut self) {
        if !self.breakpoints_need_update {
            return;
        }
    
        // Create a Vec to store all our opcode indexes
        let mut opcode_indexes = Vec::new();
    
        // First, collect all the source path and line number pairs we need to look up
        let breakpoint_locations: Vec<_> = self
            .breakpoints
            .iter()
            .flat_map(|(source_path, breakpoints)| {
                breakpoints
                    .iter()
                    .filter_map(|bp| bp.line.map(|line| (source_path.clone(), line)))
                    .collect::<Vec<_>>()
            })
            .collect();
    
        // Now look up each location in the source map
        for (source_path, line) in breakpoint_locations {
            if let Some(pc) = self
                .source_map
                .map
                .iter()
                .find_map(|(pc, span)| {
                    let path = &self.source_map.paths[span.path.0];
                    if path == &source_path && span.range.start.line as i64 == line {
                        Some(*pc)
                    } else {
                        None
                    }
                })
            {
                opcode_indexes.push(pc);
            }
        }
    
        // Update the breakpoints in each executor
        for executor in &mut self.executors {
            // TODO: use `overwrite_breakpoints` when released
            for &opcode_index in &opcode_indexes {
                let bp = fuel_vm::state::Breakpoint::script(opcode_index as u64);
                executor.interpreter.set_breakpoint(bp);
            }
        }
    
        self.breakpoints_need_update = false;
    }
    // pub(crate) fn update_vm_breakpoints(&mut self) {
    //     if !self.breakpoints_need_update {
    //         return;
    //     }
    //     let opcode_indexes = self
    //         .breakpoints
    //         .iter()
    //         .flat_map(|(source_path, breakpoints)| {
    //             if let Some(source_map) = self.source_map.get(&PathBuf::from(source_path)) {
    //                 breakpoints
    //                     .iter()
    //                     .filter_map(|bp| {
    //                         bp.line.and_then(|line| {
    //                             source_map
    //                                 .get(&line)
    //                                 .and_then(|instructions| instructions.first())
    //                         })
    //                     })
    //                     .collect::<Vec<_>>()
    //             } else {
    //                 vec![]
    //             }
    //         });

    //     self.executors.iter_mut().for_each(|executor| {
    //         // TODO: use `overwrite_breakpoints` when released
    //         opcode_indexes.clone().for_each(|opcode_index| {
    //             let bp: fuel_vm::prelude::Breakpoint =
    //                 fuel_vm::state::Breakpoint::script(*opcode_index);
    //             executor.interpreter.set_breakpoint(bp);
    //         });
    //     });
    // }

    pub(crate) fn test_complete(&mut self, result: TestResult) {
        self.test_results.push(result);
        self.executors.remove(0);
    }
}
