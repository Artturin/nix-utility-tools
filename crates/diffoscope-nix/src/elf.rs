//! Locating GNU build IDs in ELF images.

use std::collections::BTreeSet;
use std::ops::Range;

use goblin::elf::Elf;
use goblin::elf::note::NT_GNU_BUILD_ID;
use goblin::elf::program_header::ProgramHeader;
use goblin::elf::section_header::SectionHeader;

/// Byte ranges of the descriptor of every NT_GNU_BUILD_ID note in an ELF
/// image that isn't all zeros already. Zeroing them keeps the length, so
/// nothing moves. Notes are found through both the PT_NOTE segments and the
/// SHT_NOTE sections, so files without sections work too.
pub fn nonzero_build_ids(data: &[u8]) -> Vec<Range<usize>> {
    if !data.starts_with(b"\x7fELF") {
        return Vec::new();
    }
    let Some(elf) = parse_headers(data) else {
        return Vec::new();
    };

    let notes = elf
        .iter_note_headers(data)
        .into_iter()
        .chain(elf.iter_note_sections(data, None))
        .flatten()
        .flatten();
    let mut found = BTreeSet::new();
    for note in notes {
        if note.n_type != NT_GNU_BUILD_ID || note.name != "GNU" {
            continue;
        }
        // The descriptor is a subslice of `data`, so its position is the
        // difference between the addresses.
        let Some(start) = (note.desc.as_ptr() as usize).checked_sub(data.as_ptr() as usize) else {
            continue;
        };
        let range = start..start + note.desc.len();
        if note.desc.iter().any(|&b| b != 0) {
            found.insert((range.start, range.end));
        }
    }
    found.into_iter().map(|(start, end)| start..end).collect()
}

/// Only the ELF, program and section headers: enough to find the notes
/// without parsing symbols, dynamic tables and so on.
fn parse_headers(data: &[u8]) -> Option<Elf<'_>> {
    let header = Elf::parse_header(data).ok()?;
    let ctx = goblin::container::Ctx::new(header.container().ok()?, header.endianness().ok()?);
    let mut elf = Elf::lazy_parse(header).ok()?;
    elf.program_headers =
        ProgramHeader::parse(data, header.e_phoff as usize, header.e_phnum as usize, ctx).ok()?;
    elf.section_headers =
        SectionHeader::parse(data, header.e_shoff as usize, header.e_shnum as usize, ctx)
            .unwrap_or_default();
    Some(elf)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A minimal 64-bit little-endian ELF with one PT_NOTE program header
    /// pointing at a single GNU build ID note.
    fn elf_with_build_id(id: &[u8]) -> Vec<u8> {
        let mut e = vec![0u8; 64 + 56];
        e[..4].copy_from_slice(b"\x7fELF");
        e[4] = 2;
        e[5] = 1;
        e[0x20..0x28].copy_from_slice(&64u64.to_le_bytes()); // phoff
        e[0x36..0x38].copy_from_slice(&56u16.to_le_bytes()); // phentsize
        e[0x38..0x3A].copy_from_slice(&1u16.to_le_bytes()); // phnum
        let note_off = e.len() as u64;
        let note_size = 12 + 4 + id.len() as u64;
        e[64..68].copy_from_slice(&4u32.to_le_bytes()); // PT_NOTE
        e[64 + 8..64 + 16].copy_from_slice(&note_off.to_le_bytes());
        e[64 + 32..64 + 40].copy_from_slice(&note_size.to_le_bytes());
        e[64 + 48..64 + 56].copy_from_slice(&4u64.to_le_bytes());
        e.extend_from_slice(&4u32.to_le_bytes());
        e.extend_from_slice(&(id.len() as u32).to_le_bytes());
        e.extend_from_slice(&3u32.to_le_bytes());
        e.extend_from_slice(b"GNU\0");
        e.extend_from_slice(id);
        e
    }

    #[test]
    fn finds_nonzero_build_id() {
        let elf = elf_with_build_id(&[1, 2, 3, 4, 5, 6, 7, 8]);
        let found = nonzero_build_ids(&elf);
        assert_eq!(found.len(), 1);
        assert_eq!(&elf[found[0].clone()], &[1, 2, 3, 4, 5, 6, 7, 8]);
    }

    #[test]
    fn ignores_zeroed_build_id_and_non_elf() {
        assert!(nonzero_build_ids(&elf_with_build_id(&[0; 8])).is_empty());
        assert!(
            nonzero_build_ids(b"not an elf file at all, just text........................")
                .is_empty()
        );
    }
}
