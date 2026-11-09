//! Just enough of the process tree to tell which session a pid runs in.

/// Deeper than any shell, wrapper and agent chain.
const MAX_DEPTH: usize = 64;

/// Whether `pid` is `ancestor` or runs below it.
pub fn descends_from(pid: u32, ancestor: u32) -> bool {
    let mut current = pid;
    for _ in 0..MAX_DEPTH {
        if current == ancestor {
            return true;
        }
        match parent(current) {
            Some(parent) if parent > 1 && parent != current => current = parent,
            _ => return false,
        }
    }
    false
}

#[cfg(target_os = "macos")]
fn parent(pid: u32) -> Option<u32> {
    // SAFETY: proc_bsdinfo is plain old data; all zeroes is a valid value.
    let mut info: libc::proc_bsdinfo = unsafe { std::mem::zeroed() };
    let size = std::mem::size_of::<libc::proc_bsdinfo>() as libc::c_int;
    // SAFETY: proc_pidinfo writes at most `size` bytes into `info`.
    let written = unsafe {
        libc::proc_pidinfo(
            pid as libc::c_int,
            libc::PROC_PIDTBSDINFO,
            0,
            (&raw mut info).cast(),
            size,
        )
    };
    (written == size).then_some(info.pbi_ppid)
}

#[cfg(not(target_os = "macos"))]
fn parent(pid: u32) -> Option<u32> {
    let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    stat.rsplit_once(')')?
        .1
        .split_whitespace()
        .nth(1)?
        .parse()
        .ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_child_descends_from_this_process_and_not_the_other_way() {
        let mut child = std::process::Command::new("sleep")
            .arg("5")
            .spawn()
            .unwrap();
        let own = std::process::id();

        assert!(descends_from(child.id(), own));
        assert!(!descends_from(own, child.id()));

        child.kill().unwrap();
        child.wait().unwrap();
        assert!(!descends_from(child.id(), own));
    }
}
