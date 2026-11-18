//! The render node as Linux shows it: `/dev/dri/renderD128`, a character
//! device (226, 128), and the sysfs a DRM client reads to find it
//! (libdrm's `drmGetDevices2`: `/sys/dev/char/226:128/device/{drm,
//! subsystem, uevent, vendor, device, ...}`), as a virtio-gpu on PCI
//! (1af4:1050), which is what Mesa's Venus looks for.
//!
//! Only while the GPU is on (`super::enabled`). `/sys` is then this tree
//! alone: there's no other sysfs on the host's root, and a distribution's
//! root has an empty directory there.

/// The render node's device number.
pub const MAJOR: u32 = 226;
pub const MINOR: u32 = 128;

/// What a path in the tree is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Node {
    Dir(&'static [(&'static str, u8)]),
    File(&'static [u8]),
    Link(&'static str),
    /// The render node itself.
    Render,
}

const DT_DIR: u8 = libc::DT_DIR;
const DT_REG: u8 = libc::DT_REG;
const DT_LNK: u8 = libc::DT_LNK;
const DT_CHR: u8 = libc::DT_CHR;

/// PCI config space's first 64 bytes: vendor 1af4, device 1050, revision
/// 1, class 0x038000 (display, other), subsystem 1af4:1100.
const CONFIG: [u8; 64] = {
    let mut c = [0u8; 64];
    c[0] = 0xf4;
    c[1] = 0x1a;
    c[2] = 0x50;
    c[3] = 0x10;
    c[8] = 0x01;
    c[10] = 0x80;
    c[11] = 0x03;
    c[44] = 0xf4;
    c[45] = 0x1a;
    c[46] = 0x00;
    c[47] = 0x11;
    c
};

/// The tree, by absolute guest path.
pub fn lookup(path: &[u8]) -> Option<Node> {
    Some(match path {
        b"/dev/dri" => Node::Dir(&[("renderD128", DT_CHR), ("by-path", DT_DIR)]),
        b"/dev/dri/by-path" => Node::Dir(&[("pci-0000:00:02.0-render", DT_LNK)]),
        b"/dev/dri/by-path/pci-0000:00:02.0-render" => Node::Link("../renderD128"),
        b"/dev/dri/renderD128" => Node::Render,
        b"/sys" => Node::Dir(&[("dev", DT_DIR), ("class", DT_DIR), ("bus", DT_DIR)]),
        b"/sys/dev" => Node::Dir(&[("char", DT_DIR)]),
        b"/sys/dev/char" => Node::Dir(&[("226:128", DT_DIR)]),
        b"/sys/dev/char/226:128" => Node::Dir(&[("dev", DT_REG), ("uevent", DT_REG), ("device", DT_DIR), ("subsystem", DT_LNK)]),
        b"/sys/dev/char/226:128/dev" => Node::File(b"226:128\n"),
        b"/sys/dev/char/226:128/uevent" => Node::File(b"MAJOR=226\nMINOR=128\nDEVNAME=dri/renderD128\nDEVTYPE=drm_minor\n"),
        b"/sys/dev/char/226:128/subsystem" => Node::Link("../../../class/drm"),
        b"/sys/dev/char/226:128/device" => Node::Dir(&[
            ("drm", DT_DIR),
            ("subsystem", DT_LNK),
            ("uevent", DT_REG),
            ("vendor", DT_REG),
            ("device", DT_REG),
            ("subsystem_vendor", DT_REG),
            ("subsystem_device", DT_REG),
            ("revision", DT_REG),
            ("class", DT_REG),
            ("config", DT_REG),
        ]),
        b"/sys/dev/char/226:128/device/drm" => Node::Dir(&[("renderD128", DT_DIR)]),
        b"/sys/dev/char/226:128/device/drm/renderD128" => Node::Dir(&[("dev", DT_REG), ("uevent", DT_REG)]),
        b"/sys/dev/char/226:128/device/drm/renderD128/dev" => Node::File(b"226:128\n"),
        b"/sys/dev/char/226:128/device/drm/renderD128/uevent" => Node::File(b"MAJOR=226\nMINOR=128\nDEVNAME=dri/renderD128\nDEVTYPE=drm_minor\n"),
        b"/sys/dev/char/226:128/device/subsystem" => Node::Link("../../../../bus/pci"),
        b"/sys/dev/char/226:128/device/uevent" => Node::File(
            b"DRIVER=virtio-pci\nPCI_CLASS=38000\nPCI_ID=1AF4:1050\nPCI_SUBSYS_ID=1AF4:1100\nPCI_SLOT_NAME=0000:00:02.0\nMODALIAS=pci:v00001AF4d00001050sv00001AF4sd00001100bc03sc80i00\n",
        ),
        b"/sys/dev/char/226:128/device/vendor" => Node::File(b"0x1af4\n"),
        b"/sys/dev/char/226:128/device/device" => Node::File(b"0x1050\n"),
        b"/sys/dev/char/226:128/device/subsystem_vendor" => Node::File(b"0x1af4\n"),
        b"/sys/dev/char/226:128/device/subsystem_device" => Node::File(b"0x1100\n"),
        b"/sys/dev/char/226:128/device/revision" => Node::File(b"0x01\n"),
        b"/sys/dev/char/226:128/device/class" => Node::File(b"0x038000\n"),
        b"/sys/dev/char/226:128/device/config" => Node::File(&CONFIG),
        b"/sys/class" => Node::Dir(&[("drm", DT_DIR)]),
        b"/sys/class/drm" => Node::Dir(&[("renderD128", DT_LNK)]),
        b"/sys/class/drm/renderD128" => Node::Link("../../dev/char/226:128"),
        b"/sys/bus" => Node::Dir(&[("pci", DT_DIR)]),
        b"/sys/bus/pci" => Node::Dir(&[]),
        _ => return None,
    })
}

/// Whether `path` (absolute, normalized) is in the tree's part of the
/// namespace: `/sys` and below, `/dev/dri` and below.
pub fn covers(path: &[u8]) -> bool {
    path == b"/sys" || path.starts_with(b"/sys/") || path == b"/dev/dri" || path.starts_with(b"/dev/dri/")
}

/// `path` (absolute, normalized, in the tree's part of the namespace) with
/// the tree's links followed: all but the last component's, and that too
/// if `follow`. ENOENT if a component is missing, ENOTDIR if one isn't a
/// directory.
pub fn resolve(path: &[u8], follow: bool) -> Result<Vec<u8>, i64> {
    let mut todo: Vec<Vec<u8>> = path.split(|&b| b == b'/').filter(|c| !c.is_empty()).rev().map(|c| c.to_vec()).collect();
    let mut done: Vec<Vec<u8>> = Vec::new();
    let mut links = 0;
    while let Some(c) = todo.pop() {
        match c.as_slice() {
            b"." => continue,
            b".." => {
                done.pop();
                continue;
            }
            _ => {}
        }
        done.push(c);
        let here = join(&done);
        match lookup(&here) {
            Some(Node::Link(target)) if follow || !todo.is_empty() => {
                links += 1;
                if links > 40 {
                    return Err(crate::errno::ELOOP);
                }
                done.pop();
                todo.extend(target.split('/').filter(|c| !c.is_empty()).rev().map(|c| c.as_bytes().to_vec()));
            }
            Some(Node::Dir(_)) => {}
            Some(_) if todo.is_empty() => {}
            Some(_) => return Err(crate::errno::ENOTDIR),
            // `/dev`, above the tree.
            None if !covers(&here) => {}
            None => return Err(crate::errno::ENOENT),
        }
    }
    Ok(join(&done))
}

fn join(parts: &[Vec<u8>]) -> Vec<u8> {
    if parts.is_empty() {
        return b"/".to_vec();
    }
    parts.iter().flat_map(|p| [&b"/"[..], p].concat()).collect()
}

/// `stat` of a node.
pub fn stat(node: Node) -> libc::stat {
    let mut st: libc::stat = unsafe { std::mem::zeroed() };
    st.st_uid = 0;
    st.st_gid = 0;
    st.st_nlink = 1;
    match node {
        Node::Dir(_) => {
            st.st_mode = libc::S_IFDIR | 0o755;
            st.st_nlink = 2;
        }
        Node::File(contents) => {
            st.st_mode = libc::S_IFREG | 0o444;
            st.st_size = contents.len() as i64;
        }
        Node::Link(target) => {
            st.st_mode = libc::S_IFLNK | 0o777;
            st.st_size = target.len() as i64;
        }
        Node::Render => {
            // Anyone may open it, as on a desktop distribution (render
            // group, or uaccess).
            st.st_mode = libc::S_IFCHR | 0o666;
            // Darwin's dev_t: 8-bit major, 24-bit minor (`write_stat`
            // makes Linux's from it).
            st.st_rdev = ((MAJOR << 24) | MINOR) as libc::dev_t;
        }
    }
    st
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_listed_child_exists() {
        let mut dirs = vec![b"/sys".to_vec(), b"/dev/dri".to_vec()];
        while let Some(dir) = dirs.pop() {
            let Some(Node::Dir(children)) = lookup(&dir) else { panic!("{} isn't a directory", String::from_utf8_lossy(&dir)) };
            for (name, kind) in children {
                let path = [&dir[..], b"/", name.as_bytes()].concat();
                let node = lookup(&path).unwrap_or_else(|| panic!("{} is listed but missing", String::from_utf8_lossy(&path)));
                let want = match node {
                    Node::Dir(_) => DT_DIR,
                    Node::File(_) => DT_REG,
                    Node::Link(_) => DT_LNK,
                    Node::Render => DT_CHR,
                };
                assert_eq!(*kind, want, "{}", String::from_utf8_lossy(&path));
                if want == DT_DIR {
                    dirs.push(path);
                }
            }
        }
    }

    #[test]
    fn links_are_followed() {
        assert_eq!(resolve(b"/sys/class/drm/renderD128/device/vendor", true).unwrap(), b"/sys/dev/char/226:128/device/vendor");
        assert_eq!(resolve(b"/sys/class/drm/renderD128", false).unwrap(), b"/sys/class/drm/renderD128");
        assert_eq!(resolve(b"/sys/class/drm/renderD128", true).unwrap(), b"/sys/dev/char/226:128");
        assert_eq!(resolve(b"/dev/dri/by-path/pci-0000:00:02.0-render", true).unwrap(), b"/dev/dri/renderD128");
        assert_eq!(resolve(b"/sys/dev/char/226:128/device/vendor/x", true), Err(crate::errno::ENOTDIR));
        assert_eq!(resolve(b"/sys/nothing", true), Err(crate::errno::ENOENT));
    }

    #[test]
    fn the_render_node_is_226_128() {
        let st = stat(Node::Render);
        assert_eq!(st.st_mode & libc::S_IFMT, libc::S_IFCHR);
        assert_eq!((st.st_rdev as u32 >> 24, st.st_rdev as u32 & 0xff_ffff), (226, 128));
    }
}
