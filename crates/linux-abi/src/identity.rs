//! The user, as Linux programs look them up: `/etc/passwd` and `/etc/group`
//! read through the guest get the user's own entry and groups added, from
//! macOS (whose `/etc/passwd` doesn't list ordinary users, and a distro's
//! root doesn't know them at all). Otherwise shells say "I have no name!".

use std::ffi::CStr;

/// Whether [`extend`] changes the file at `guest_path`, so it's worth reading.
pub fn extends(guest_path: &[u8]) -> bool {
    matches!(guest_path, b"/etc/passwd" | b"/etc/group")
}

/// What to add to `file` (the guest's `/etc/passwd` or `/etc/group`), given
/// its current contents, if it's one of them. `shell` says whether a path
/// exists in the guest (for the login shell).
pub fn extend(guest_path: &[u8], contents: &[u8], exists: impl Fn(&[u8]) -> bool) -> Option<Vec<u8>> {
    let listed = |field: usize, id: u32| {
        contents.split(|&b| b == b'\n').any(|line| line.split(|&b| b == b':').nth(field) == Some(id.to_string().as_bytes()))
    };
    let mut out = contents.to_vec();
    if !out.is_empty() && !out.ends_with(b"\n") {
        out.push(b'\n');
    }
    match guest_path {
        b"/etc/passwd" => {
            let uid = unsafe { libc::getuid() };
            if !listed(2, uid) {
                let pw = unsafe { libc::getpwuid(uid) };
                let field = |p: *const libc::c_char, default: &str| {
                    if p.is_null() { default.into() } else { unsafe { CStr::from_ptr(p) }.to_string_lossy().into_owned() }
                };
                let (name, gecos, home) = if pw.is_null() {
                    ("user".into(), String::new(), std::env::var("HOME").unwrap_or_else(|_| "/".into()))
                } else {
                    let pw = unsafe { &*pw };
                    (field(pw.pw_name, "user"), field(pw.pw_gecos, ""), field(pw.pw_dir, "/"))
                };
                let shell = ["/bin/bash", "/usr/bin/bash", "/bin/sh"].into_iter().find(|s| exists(s.as_bytes())).unwrap_or("/bin/sh");
                let gid = unsafe { libc::getgid() };
                out.extend_from_slice(format!("{name}:x:{uid}:{gid}:{gecos}:{home}:{shell}\n").as_bytes());
            }
            Some(out)
        }
        b"/etc/group" => {
            let user = unsafe { libc::getpwuid(libc::getuid()) };
            let user = if user.is_null() { String::new() } else { unsafe { CStr::from_ptr((*user).pw_name) }.to_string_lossy().into_owned() };
            let n = unsafe { libc::getgroups(0, std::ptr::null_mut()) }.max(0) as usize;
            let mut gids = vec![0 as libc::gid_t; n];
            let n = unsafe { libc::getgroups(n as i32, gids.as_mut_ptr()) }.max(0) as usize;
            gids.truncate(n);
            gids.push(unsafe { libc::getgid() });
            gids.sort_unstable();
            gids.dedup();
            for gid in gids {
                if listed(2, gid) {
                    continue;
                }
                let gr = unsafe { libc::getgrgid(gid) };
                if gr.is_null() {
                    continue;
                }
                let name = unsafe { CStr::from_ptr((*gr).gr_name) }.to_string_lossy();
                out.extend_from_slice(format!("{name}:x:{gid}:{user}\n").as_bytes());
            }
            Some(out)
        }
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn adds_the_user_and_their_groups() {
        let passwd = extend(b"/etc/passwd", b"root:x:0:0:root:/root:/bin/bash\n", |p| p == b"/bin/bash").unwrap();
        let text = String::from_utf8(passwd).unwrap();
        let uid = unsafe { libc::getuid() };
        let me = text.lines().find(|l| l.split(':').nth(2) == Some(&uid.to_string())).expect("our entry");
        assert!(me.ends_with(":/bin/bash"));
        assert!(text.starts_with("root:x:0:0"), "the original stays");
        // Already listed: left alone.
        assert_eq!(extend(b"/etc/passwd", me.as_bytes(), |_| true).unwrap(), format!("{me}\n").as_bytes());
        let group = String::from_utf8(extend(b"/etc/group", b"", |_| true).unwrap()).unwrap();
        let gid = unsafe { libc::getgid() };
        assert!(group.lines().any(|l| l.split(':').nth(2) == Some(&gid.to_string())));
        assert_eq!(extend(b"/etc/hosts", b"x", |_| true), None);
    }
}
