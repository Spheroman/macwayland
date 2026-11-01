//! Load an arm64 Linux ELF (and its dynamic linker, if it has one) and build
//! its initial stack.

use std::fmt;

use linux_abi::abi::auxv;

use crate::{
    memory::{AddressSpace, STACK_TOP},
    paging::{prot, PAGE_SIZE},
};

const EM_AARCH64: u16 = 183;
const ET_EXEC: u16 = 2;
const ET_DYN: u16 = 3;
const PT_LOAD: u32 = 1;
const PT_INTERP: u32 = 3;
const PT_PHDR: u32 = 6;
const PF_X: u32 = 1;
const PF_W: u32 = 2;
const PF_R: u32 = 4;
const EHDR_SIZE: usize = 64;
const PHDR_SIZE: usize = 56;

/// Where a position-independent program (ET_DYN) is loaded: Linux's
/// ELF_ET_DYN_BASE for 48-bit address spaces (2/3 of the way up).
const PIE_BASE: u64 = 0xaaaa_aaaa_0000;

#[derive(Debug)]
pub enum LoadError {
    NotElf,
    Unsupported(&'static str),
    /// The program's dynamic linker (PT_INTERP) couldn't be read.
    Interpreter(String, std::io::Error),
    Truncated,
    /// A segment or the stack doesn't fit in guest user memory, or segments overlap.
    OutOfRange(u64),
}

impl fmt::Display for LoadError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            LoadError::NotElf => write!(f, "not an ELF file"),
            LoadError::Unsupported(what) => write!(f, "unsupported ELF: {what}"),
            LoadError::Interpreter(path, e) => write!(f, "dynamic linker {path}: {e}"),
            LoadError::Truncated => write!(f, "ELF file is truncated"),
            LoadError::OutOfRange(addr) => write!(f, "address {addr:#x} is outside guest user memory"),
        }
    }
}

impl std::error::Error for LoadError {}

/// What the loader found, for the initial stack and the vCPU.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Image {
    pub entry: u64,
    /// Guest address of the program headers, for AT_PHDR.
    pub phdr: u64,
    pub phnum: u64,
    /// Where the image was loaded relative to its link addresses (0 for
    /// ET_EXEC); for the dynamic linker, AT_BASE.
    pub bias: u64,
    /// End of the highest segment; the program break starts here.
    pub end: u64,
    /// The dynamic linker this program asks for (PT_INTERP), if any.
    pub interp: Option<Vec<u8>>,
}

fn u16_at(d: &[u8], off: usize) -> u16 {
    u16::from_le_bytes(d[off..off + 2].try_into().unwrap())
}
fn u32_at(d: &[u8], off: usize) -> u32 {
    u32::from_le_bytes(d[off..off + 4].try_into().unwrap())
}
fn u64_at(d: &[u8], off: usize) -> u64 {
    u64::from_le_bytes(d[off..off + 8].try_into().unwrap())
}

/// Load a program: an ET_EXEC at its link address, or a PIE (ET_DYN) at
/// [`PIE_BASE`]. If it names a dynamic linker, load that with
/// [`load_interpreter`] and start there.
pub fn load(space: &mut AddressSpace, elf: &[u8]) -> Result<Image, LoadError> {
    load_at(space, elf, |_, _| Ok(PIE_BASE))
}

/// Load a dynamic linker (an ET_DYN) where a mapping with no address would
/// go, top-down below the stack gap, as Linux does.
pub fn load_interpreter(space: &mut AddressSpace, elf: &[u8]) -> Result<Image, LoadError> {
    let image = load_at(space, elf, |space, span| space.find_free(span).ok_or(LoadError::OutOfRange(span)))?;
    if image.interp.is_some() {
        return Err(LoadError::Unsupported("the dynamic linker itself names a dynamic linker"));
    }
    Ok(image)
}

/// Map the PT_LOAD segments of `elf` into the address space and copy them in.
/// For an ET_DYN, `place` gets the span of its segments and returns the bias.
fn load_at(
    space: &mut AddressSpace,
    elf: &[u8],
    place: impl FnOnce(&AddressSpace, u64) -> Result<u64, LoadError>,
) -> Result<Image, LoadError> {
    if elf.len() < EHDR_SIZE || &elf[..4] != b"\x7fELF" {
        return Err(LoadError::NotElf);
    }
    if elf[4] != 2 || elf[5] != 1 {
        return Err(LoadError::Unsupported("not 64-bit little-endian"));
    }
    if u16_at(elf, 18) != EM_AARCH64 {
        return Err(LoadError::Unsupported("not arm64"));
    }
    let phoff = u64_at(elf, 32) as usize;
    let phentsize = u16_at(elf, 54) as usize;
    let phnum = u16_at(elf, 56) as usize;
    if phentsize != PHDR_SIZE {
        return Err(LoadError::Unsupported("unexpected program header size"));
    }
    let phdrs = phoff
        .checked_add(phnum * PHDR_SIZE)
        .and_then(|phend| elf.get(phoff..phend))
        .ok_or(LoadError::Truncated)?;
    let loads = || phdrs.chunks_exact(PHDR_SIZE).filter(|ph| u32_at(ph, 0) == PT_LOAD);
    let bias = match u16_at(elf, 16) {
        ET_EXEC => 0,
        ET_DYN => {
            let low = loads().map(|ph| u64_at(ph, 16)).min().ok_or(LoadError::Unsupported("no loadable segments"))?;
            let high = loads().map(|ph| u64_at(ph, 16).saturating_add(u64_at(ph, 40))).max().unwrap();
            let low = low & !(PAGE_SIZE - 1);
            let span = high.checked_next_multiple_of(PAGE_SIZE).ok_or(LoadError::OutOfRange(high))? - low;
            place(space, span)?.wrapping_sub(low)
        }
        _ => return Err(LoadError::Unsupported("not an executable")),
    };
    let entry = u64_at(elf, 24).wrapping_add(bias);

    let mut phdr_addr = None;
    let mut interp = None;
    let mut end = 0;
    for ph in phdrs.chunks_exact(PHDR_SIZE) {
        let kind = u32_at(ph, 0);
        let flags = u32_at(ph, 4);
        let offset = u64_at(ph, 8);
        let vaddr = u64_at(ph, 16).wrapping_add(bias);
        let filesz = u64_at(ph, 32);
        let memsz = u64_at(ph, 40);
        let contents = || {
            offset
                .checked_add(filesz)
                .and_then(|file_end| elf.get(offset as usize..file_end as usize))
                .ok_or(LoadError::Truncated)
        };
        match kind {
            PT_INTERP => {
                let path = contents()?;
                interp = Some(path.split(|&b| b == 0).next().unwrap_or_default().to_vec());
            }
            PT_PHDR => phdr_addr = Some(vaddr),
            PT_LOAD => {
                let seg_end = vaddr.checked_add(memsz).ok_or(LoadError::OutOfRange(vaddr))?;
                if filesz > memsz {
                    return Err(LoadError::Unsupported("segment file size exceeds memory size"));
                }
                let p = [(PF_R, prot::READ), (PF_W, prot::WRITE), (PF_X, prot::EXEC)]
                    .iter()
                    .filter(|&&(pf, _)| flags & pf != 0)
                    .fold(0, |acc, &(_, p)| acc | p);
                let page_end = seg_end.checked_next_multiple_of(PAGE_SIZE).ok_or(LoadError::OutOfRange(vaddr))?;
                space.map_segment(vaddr & !(PAGE_SIZE - 1), page_end, p).map_err(LoadError::OutOfRange)?;
                space.poke(vaddr, contents()?).map_err(|f| LoadError::OutOfRange(f.addr))?;
                // The rest (.bss) is zero: fresh pages are.
                // The program headers usually sit inside the first segment.
                if phdr_addr.is_none() && (offset..offset + filesz).contains(&(phoff as u64)) {
                    phdr_addr = Some(vaddr + (phoff as u64 - offset));
                }
                end = end.max(seg_end);
            }
            _ => {}
        }
    }
    if end == 0 {
        return Err(LoadError::Unsupported("no loadable segments"));
    }
    Ok(Image { entry, phdr: phdr_addr.unwrap_or(0), phnum: phnum as u64, bias, end, interp })
}

/// Build the stack Linux hands a new program and return the initial SP:
///
/// ```text
/// sp → argc, argv[0..argc], NULL, envp[..], NULL, auxv pairs, AT_NULL
///      ... then the strings and AT_RANDOM's 16 bytes, up to STACK_TOP
/// ```
///
/// `interp` is the dynamic linker loaded for `image`, if any (AT_BASE).
pub fn build_stack(
    space: &mut AddressSpace,
    image: &Image,
    interp: Option<&Image>,
    argv: &[&[u8]],
    envp: &[&[u8]],
) -> Result<u64, LoadError> {
    space.map_stack();
    let mut top = STACK_TOP;
    let mut push = |space: &mut AddressSpace, bytes: &[u8]| -> Result<u64, LoadError> {
        top -= bytes.len() as u64;
        space.write(top, bytes).map_err(|f| LoadError::OutOfRange(f.addr))?;
        Ok(top)
    };
    let mut push_str = |space: &mut AddressSpace, s: &[u8]| {
        let mut owned = s.to_vec();
        owned.push(0);
        push(space, &owned)
    };
    let argv_ptrs = argv.iter().map(|s| push_str(space, s)).collect::<Result<Vec<_>, _>>()?;
    let envp_ptrs = envp.iter().map(|s| push_str(space, s)).collect::<Result<Vec<_>, _>>()?;
    let mut random = [0u8; 16];
    unsafe { libc::arc4random_buf(random.as_mut_ptr().cast(), random.len()) };
    let random_ptr = push(space, &random)?;

    let auxv = [
        (auxv::PHDR, image.phdr),
        (auxv::PHENT, 56),
        (auxv::PHNUM, image.phnum),
        (auxv::PAGESZ, PAGE_SIZE),
        (auxv::BASE, interp.map_or(0, |i| i.bias)),
        (auxv::FLAGS, 0),
        (auxv::ENTRY, image.entry),
        (auxv::UID, unsafe { libc::getuid() } as u64),
        (auxv::EUID, unsafe { libc::geteuid() } as u64),
        (auxv::GID, unsafe { libc::getgid() } as u64),
        (auxv::EGID, unsafe { libc::getegid() } as u64),
        (auxv::HWCAP, auxv::HWCAP_FP | auxv::HWCAP_ASIMD),
        (auxv::CLKTCK, 100),
        (auxv::SECURE, 0),
        (auxv::RANDOM, random_ptr),
        (auxv::EXECFN, argv_ptrs.first().copied().unwrap_or(0)),
        (auxv::NULL, 0),
    ];

    let mut words = vec![argv.len() as u64];
    words.extend(&argv_ptrs);
    words.push(0);
    words.extend(&envp_ptrs);
    words.push(0);
    words.extend(auxv.iter().flat_map(|&(k, v)| [k, v]));

    // The arm64 ABI wants SP 16-byte aligned at entry.
    let size = words.len() as u64 * 8;
    let sp = (top - size) & !15;
    let bytes: Vec<u8> = words.iter().flat_map(|w| w.to_le_bytes()).collect();
    space.write(sp, &bytes).map_err(|f| LoadError::OutOfRange(f.addr))?;
    Ok(sp)
}
