//! Exercise OS resource locks through separate processes, including abrupt death.
use nuphus_browser::coordination::Coordination;
use std::{
    path::PathBuf,
    process::{Child, Command},
    time::{Duration, Instant},
};

struct Sandbox(PathBuf);
impl Sandbox {
    fn new(name: &str) -> Self {
        let root =
            std::env::temp_dir().join(format!("nuphus-coordination-{}-{name}", std::process::id()));
        std::fs::create_dir_all(&root).unwrap();
        Self(root)
    }
    fn coordinator(&self) -> Coordination {
        Coordination::at(&self.0)
    }
    fn worker(&self, name: &str, kind: &str) -> Worker {
        let ready = self.0.join(format!("{name}.ready"));
        let _ = std::fs::remove_file(&ready);
        let child = Command::new(std::env::current_exe().unwrap())
            .args(["--exact", "lock_worker", "--nocapture"])
            .env("NUPHUS_TEST_LOCK_ROOT", &self.0)
            .env("NUPHUS_TEST_LOCK_KIND", kind)
            .env("NUPHUS_TEST_LOCK_READY", &ready)
            .stdout(std::process::Stdio::null())
            .spawn()
            .unwrap();
        let mut worker = Worker(child);
        let deadline = Instant::now() + Duration::from_secs(10);
        while !ready.exists() {
            assert!(
                worker.0.try_wait().unwrap().is_none(),
                "worker failed to acquire {kind}"
            );
            assert!(Instant::now() < deadline, "worker did not become ready");
            std::thread::sleep(Duration::from_millis(10));
        }
        worker
    }
}
impl Drop for Sandbox {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}
struct Worker(Child);
impl Drop for Worker {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

#[test]
fn lock_worker() {
    let Ok(root) = std::env::var("NUPHUS_TEST_LOCK_ROOT") else {
        return;
    };
    let c = Coordination::at(root);
    let kind = std::env::var("NUPHUS_TEST_LOCK_KIND").unwrap();
    let _guard = match kind.as_str() {
        "background" => c.machine(false),
        "desktop" => c.machine(true),
        "tab-a" => c.tab("browser-one", "a"),
        "browser-read" => c.browser("browser-one", false),
        _ => panic!("unknown worker"),
    }
    .unwrap();
    std::fs::write(std::env::var("NUPHUS_TEST_LOCK_READY").unwrap(), b"ready").unwrap();
    loop {
        std::thread::park();
    }
}

#[test]
fn separate_background_processes_share_machine_and_exclude_desktop() {
    let s = Sandbox::new("machine");
    let c = s.coordinator();
    let a = s.worker("a", "background");
    let b = s.worker("b", "background");
    assert!(c.machine(false).is_ok());
    assert!(c.machine(true).unwrap_err().contains("busy"));
    drop((a, b));
    let desktop = s.worker("desktop", "desktop");
    assert!(c.machine(true).is_err());
    assert!(c.machine(false).is_err());
    drop(desktop);
    assert!(
        c.machine(true).is_ok(),
        "process death must release the gate immediately"
    );
}

#[test]
fn tab_claim_lasts_between_calls_and_is_recovered_after_crash() {
    let s = Sandbox::new("tab");
    let c = s.coordinator();
    let owner = s.worker("owner", "tab-a");
    assert!(c.tab("browser-one", "a").unwrap_err().contains("busy"));
    assert!(c.tab("browser-one", "b").is_ok());
    assert!(c.tab("browser-two", "a").is_ok());
    drop(owner);
    assert!(c.tab("browser-one", "a").is_ok());
}

#[test]
fn browser_writes_exclude_reads_but_not_another_browser() {
    let s = Sandbox::new("browser");
    let c = s.coordinator();
    let reader = s.worker("reader", "browser-read");
    assert!(c.browser("browser-one", false).is_ok());
    assert!(c.browser("browser-one", true).is_err());
    assert!(c.browser("browser-two", true).is_ok());
    drop(reader);
    let writer = c.browser("browser-one", true).unwrap();
    assert!(c.browser("browser-one", false).is_err());
    drop(writer);
    assert!(c.browser("browser-one", false).is_ok());
}
