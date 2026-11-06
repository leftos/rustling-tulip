//! The gate's live state, read through leftos/gate's `gate-state` crate.

#[cfg(test)]
mod tests {
    use std::fs;
    use std::path::PathBuf;

    use ::gate_state::{CwdReader, MutexProbe, SlotKind};
    use ::proc_table::EmptyTable;

    /// A machine with no gate slot's mutex open and no process whose working directory can be read.
    struct QuietProbe;

    impl MutexProbe for QuietProbe {
        fn is_open(&self, _name: &str) -> bool {
            false
        }
    }

    impl CwdReader for QuietProbe {
        fn current_dir(&self, _pid: u32) -> Option<String> {
            None
        }
    }

    /// The state assembly runs from this crate: two empty pools, heavy then light.
    #[test]
    fn dashboard_state_reads_two_empty_pools() -> anyhow::Result<()> {
        let root =
            option_env!("CARGO_TARGET_TMPDIR").map_or_else(std::env::temp_dir, PathBuf::from);
        let dir = root
            .join("rt-gate-state")
            .join("dashboard_state_reads_two_empty_pools");
        if dir.exists() {
            fs::remove_dir_all(&dir)?;
        }
        fs::create_dir_all(dir.join("gate").join("slots"))?;

        let inputs = ::gate_state::StateInputs {
            now: "2026-10-01T12:00:00Z".parse()?,
            machine: "TESTBOX".to_owned(),
            processors: 16,
            local_app_data: dir.clone(),
            slot_counts_override: None,
            slot_prefix: String::new(),
            generation: ::gate_state::Generation::detect(&dir)?,
        };

        let state = ::gate_state::dashboard_state(&inputs, &EmptyTable, &QuietProbe, &QuietProbe);

        anyhow::ensure!(
            state.pools.len() == 2,
            "two pools, got {}",
            state.pools.len()
        );
        anyhow::ensure!(state.pools[0].kind == SlotKind::Heavy, "heavy first");
        anyhow::ensure!(state.pools[1].kind == SlotKind::Light, "light second");
        anyhow::ensure!(
            state.machine == "TESTBOX",
            "the machine is TESTBOX, got {}",
            state.machine
        );
        Ok(())
    }
}
