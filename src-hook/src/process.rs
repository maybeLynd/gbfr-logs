use anyhow::anyhow;
use pelite::{
    pattern,
    pe64::{Pe, PeView},
};
use thiserror::Error;
use windows::Win32::Foundation::HMODULE;
use windows::Win32::System::Diagnostics::ToolHelp::{
    CreateToolhelp32Snapshot, Module32FirstW, Process32FirstW, Process32NextW, MODULEENTRY32W,
    PROCESSENTRY32W, TH32CS_SNAPMODULE, TH32CS_SNAPMODULE32, TH32CS_SNAPPROCESS,
};

#[derive(Error, Debug)]
pub enum ProcessError {
    #[error("Process was not found with that name")]
    ProcessNotFound,
    #[error("Could not snapshot process")]
    ProcessSnapshotError(windows::core::Error),
    #[error("Could not snapshot process memory")]
    ModuleSnapshotError(windows::core::Error),
}

pub struct Process {
    pub base_address: usize,
    pub module_handle: HMODULE,
}

impl Process {
    /// Finds a process by its name.
    pub fn with_name(name: &str) -> Result<Process, ProcessError> {
        let mut found_process = None;

        unsafe {
            let snapshot_handle = CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0)
                .map_err(ProcessError::ProcessSnapshotError)?;

            let mut process = PROCESSENTRY32W {
                dwSize: std::mem::size_of::<PROCESSENTRY32W>() as u32,
                ..PROCESSENTRY32W::default()
            };

            if Process32FirstW(snapshot_handle, &mut process).is_ok() {
                loop {
                    if Process32NextW(snapshot_handle, &mut process).is_ok() {
                        let process_name = String::from_utf16_lossy(&process.szExeFile)
                            .trim_end_matches('\u{0}')
                            .to_string();

                        if process_name == name {
                            let module_snapshot = CreateToolhelp32Snapshot(
                                TH32CS_SNAPMODULE | TH32CS_SNAPMODULE32,
                                process.th32ProcessID,
                            )
                            .map_err(ProcessError::ModuleSnapshotError)?;

                            let mut module_entry = MODULEENTRY32W {
                                dwSize: std::mem::size_of::<MODULEENTRY32W>() as u32,
                                ..MODULEENTRY32W::default()
                            };

                            if Module32FirstW(module_snapshot, &mut module_entry).is_ok() {
                                let module_name = String::from_utf16_lossy(&process.szExeFile)
                                    .trim_end_matches('\u{0}')
                                    .to_string();

                                if module_name == name {
                                    let base_address = module_entry.modBaseAddr as usize;
                                    let module_handle = module_entry.hModule;

                                    found_process = Some(Process {
                                        base_address,
                                        module_handle,
                                    });
                                }
                            } else {
                                break;
                            }
                        }
                    } else {
                        break;
                    }
                }
            }
        }

        found_process.ok_or(ProcessError::ProcessNotFound)
    }

    /// Searches and returns the RVAs of the function that matches the given signature pattern.
    pub fn search_address(&self, signature_pattern: &str) -> anyhow::Result<usize> {
        let view = unsafe { PeView::module(self.module_handle.0 as *const u8) };
        let scanner = view.scanner();
        let pattern = pattern::parse(signature_pattern)?;

        let mut addrs = [0; 8];

        let mut matches = scanner.matches_code(&pattern);

        let mut first_addr = None;
        let mut match_count = 0usize;

        // addrs[0] = RVA of where the match was found.
        // addrs[1] = RVA of the function being called.
        while matches.next(&mut addrs) {
            match_count += 1;
            if match_count > 1 {
                return Err(anyhow!(
                    "Signature is ambiguous ({} or more matches): {}",
                    match_count,
                    signature_pattern
                ));
            }
            first_addr = Some(self.base_address + addrs[1] as usize);
        }

        first_addr.ok_or(anyhow!(
            "Could not find match for pattern: {}",
            signature_pattern
        ))
    }

    pub fn search_match_address(&self, signature_pattern: &str) -> anyhow::Result<usize> {
        let view = unsafe { PeView::module(self.module_handle.0 as *const u8) };
        let scanner = view.scanner();
        let pattern = pattern::parse(signature_pattern)?;
        let mut addrs = [0; 8];
        let mut matches = scanner.matches_code(&pattern);

        if !matches.next(&mut addrs) {
            return Err(anyhow!(
                "Could not find match for pattern: {}",
                signature_pattern
            ));
        }

        let address = self.base_address + addrs[0] as usize;
        if matches.next(&mut addrs) {
            return Err(anyhow!(
                "Signature is ambiguous (2 or more matches): {}",
                signature_pattern
            ));
        }

        Ok(address)
    }

    pub fn search_function_start(
        &self,
        signature_pattern: &str,
        prologue: &[u8],
        max_distance: usize,
    ) -> anyhow::Result<usize> {
        if prologue.is_empty() {
            return Err(anyhow!("Function prologue cannot be empty"));
        }

        let signed_address = self.search_match_address(signature_pattern)?;
        for distance in 0..=max_distance {
            let Some(candidate) = signed_address.checked_sub(distance) else {
                break;
            };
            if candidate < self.base_address {
                break;
            }

            let bytes =
                unsafe { std::slice::from_raw_parts(candidate as *const u8, prologue.len()) };
            if bytes == prologue {
                return Ok(candidate);
            }
        }

        Err(anyhow!(
            "Could not find function prologue within {max_distance:#x} bytes of pattern: {signature_pattern}"
        ))
    }

    pub fn search_rip_relative_address(
        &self,
        signature_pattern: &str,
        displacement_offset: usize,
        instruction_size: usize,
    ) -> anyhow::Result<usize> {
        let instruction = self.search_match_address(signature_pattern)?;
        let displacement_address = instruction
            .checked_add(displacement_offset)
            .ok_or_else(|| anyhow!("RIP-relative displacement address overflow"))?;
        let displacement = unsafe { (displacement_address as *const i32).read_unaligned() };
        let next_instruction = instruction
            .checked_add(instruction_size)
            .ok_or_else(|| anyhow!("RIP-relative instruction address overflow"))?;

        next_instruction
            .checked_add_signed(displacement as isize)
            .ok_or_else(|| anyhow!("RIP-relative target address overflow"))
    }

    /// Searches and returns the value of the type `T` that matches the given signature pattern.
    pub fn search_slice<T>(&self, signature_pattern: &str) -> anyhow::Result<T> {
        let view = unsafe { PeView::module(self.module_handle.0 as *const u8) };
        let scanner = view.scanner();
        let pattern = pattern::parse(signature_pattern)?;
        let mut addrs = [0; 8];
        let mut matches = scanner.matches_code(&pattern);

        if !matches.next(&mut addrs) {
            return Err(anyhow!(
                "Could not find match for pattern: {}",
                signature_pattern
            ));
        }

        let addr = self.base_address + addrs[1] as usize;
        if matches.next(&mut addrs) {
            return Err(anyhow!(
                "Signature is ambiguous (2 or more matches): {}",
                signature_pattern
            ));
        }

        let ptr = addr as *const T;
        Ok(unsafe { ptr.read_unaligned() })
    }
}
