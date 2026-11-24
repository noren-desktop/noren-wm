use std::env;
use std::path::{Path, PathBuf};
use std::process::Command;

fn find_rc_exe() -> Option<PathBuf> {
    if let Ok(path) = Command::new("where.exe").arg("rc.exe").output() {
        if path.status.success() {
            if let Some(first) = String::from_utf8_lossy(&path.stdout).lines().next() {
                let candidate = PathBuf::from(first.trim());
                if candidate.is_file() {
                    return Some(candidate);
                }
            }
        }
    }

    let kits_bin = Path::new(r"C:\Program Files (x86)\Windows Kits\10\bin");
    if let Ok(entries) = std::fs::read_dir(kits_bin) {
        let mut versions: Vec<PathBuf> = entries
            .flatten()
            .map(|e| e.path().join("x64").join("rc.exe"))
            .filter(|p| p.is_file())
            .collect();
        versions.sort();
        if let Some(latest) = versions.pop() {
            return Some(latest);
        }
    }
    None
}

fn main() {
    println!("cargo:rerun-if-changed=assets/noren.rc");
    println!("cargo:rerun-if-changed=assets/noren.ico");

    if env::var("CARGO_CFG_TARGET_OS").as_deref() != Ok("windows") {
        return;
    }

    let Some(rc) = find_rc_exe() else {
        println!("cargo:warning=rc.exe not found; noren-wm.exe will not embed noren.ico");
        return;
    };

    let out_dir = PathBuf::from(env::var("OUT_DIR").expect("OUT_DIR missing"));
    let res_path = out_dir.join("noren.res");

    let status = Command::new(&rc)
        .args(["/nologo", "/fo"])
        .arg(&res_path)
        .arg("assets/noren.rc")
        .status();

    match status {
        Ok(st) if st.success() => {
            println!("cargo:rustc-link-arg-bins={}", res_path.display());
        }
        Ok(st) => {
            println!(
                "cargo:warning=rc.exe exited with {st}; noren-wm.exe will not embed noren.ico"
            );
        }
        Err(e) => {
            println!("cargo:warning=failed running rc.exe ({e}); noren-wm.exe will not embed noren.ico");
        }
    }
}
