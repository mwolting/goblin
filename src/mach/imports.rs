// table of tuples:
// <seg-index, seg-offset, type, symbol-library-ordinal, symbol-name, addend>
// symbol flags are undocumented 

use core::ops::Range;
use core::fmt::{self, Debug};
use scroll::{Sleb128, Uleb128, Gread, Pread};

use container;
use error;
use mach::load_command;
use mach::bind_opcodes;

#[derive(Debug)]
struct BindInformation<'a> {
  seg_index:              u8,
  seg_offset:             u64,
  bind_type:              u8,
  symbol_library_ordinal: u8,
  symbol_name:            &'a str,
  symbol_flags:           u8,
  addend:                 i64,
  special_dylib:          u8, // seeing self = 0 assuming this means the symbol is imported from itself, because its... libSystem.B.dylib?
}

impl<'a> BindInformation<'a> {
    pub fn new (is_lazy: bool) -> Self {
        let mut bind_info = BindInformation::default();
        let bind_type = if is_lazy { bind_opcodes::BIND_TYPE_POINTER } else { 0x0 };
        bind_info.bind_type = bind_type;
        bind_info
    }
    pub fn is_lazy(&self) -> bool {
        self.bind_type == bind_opcodes::BIND_TYPE_POINTER
    }
}

impl<'a> Default for BindInformation<'a> {
    fn default() -> Self {
        BindInformation {
            seg_index:     0,
            seg_offset:    0x0,
            bind_type:     0x0,
            special_dylib: 1,
            symbol_library_ordinal: 0,
            symbol_name: "",
            symbol_flags: 0,
            addend: 0
        }
    }
}

#[derive(Debug)]
pub struct Import<'a> {
    pub name: &'a str,
    pub dylib:   &'a str,
    pub is_lazy: bool,
    pub offset:  u64,
    pub size:    usize,
}

impl<'a> Import<'a> {
    fn new<'b>(bi: &BindInformation<'b>, libs: &[&'b str], segments: &[load_command::Segment]) -> Import<'b> {
        let offset = {
            let segment = &segments[bi.seg_index as usize];
            segment.fileoff + bi.seg_offset
        };
        let size = if bi.is_lazy() { 8 } else { 0 };
        Import {
            name: bi.symbol_name,
            dylib: libs[bi.symbol_library_ordinal as usize],
            is_lazy: bi.is_lazy(),
            offset: offset,
            size: size,
        }
    }
}

/// An interpreter for mach BIND opcodes.
/// Runs on prebound (non lazy) symbols (usually dylib extern consts and extern variables),
/// and lazy symbols (usually dylib functions)
pub struct BindInterpreter<'a> {
    data: &'a [u8],
    location: Range<usize>,
    lazy_location: Range<usize>,
}

impl<'a> Debug for BindInterpreter<'a> {
    fn fmt(&self, fmt: &mut fmt::Formatter) -> fmt::Result {
        writeln!(fmt, "BindInterpreter {{")?;
        writeln!(fmt, "  Location: {:#x}..{:#x}", self.location.start, self.location.end)?;
        writeln!(fmt, "  Lazy Location: {:#x}..{:#x}", self.lazy_location.start, self.lazy_location.end)?;
        writeln!(fmt, "}}")
    }
}


impl<'a> BindInterpreter<'a> {
    pub fn new<'b, B: AsRef<[u8]>> (bytes: &'b B, command: &load_command::DyldInfoCommand) -> BindInterpreter<'b> {
        let get_pos = |off: u32, size: u32| -> Range<usize> {
            off as usize..(off + size) as usize
        };
        let location = get_pos(command.bind_off, command.bind_size);
        let lazy_location = get_pos(command.lazy_bind_off, command.lazy_bind_size);
        BindInterpreter {
            data: bytes.as_ref(),
            location: location,
            lazy_location: lazy_location,
        }
    }
    pub fn imports<'b> (&'b self, libs: &[&'b str], segments: &[load_command::Segment], ctx: &container::Ctx) -> error::Result<Vec<Import<'b>>>{
        let mut imports = Vec::new();
        self.run(false, libs, segments, ctx, &mut imports)?;
        self.run( true, libs, segments, ctx, &mut imports)?;
        Ok(imports)
    }
    pub fn run<'b> (&'b self, is_lazy: bool, libs: &[&'b str], segments: &[load_command::Segment], ctx: &container::Ctx, imports: &mut Vec<Import<'b>>) -> error::Result<()>{
        use mach::bind_opcodes::*;
        let location = if is_lazy {
            &self.location
        } else {
            &self.lazy_location
        };
        let mut bind_info = BindInformation::new(is_lazy);
        let mut offset = &mut location.start.clone();
        while *offset < location.end {
            let opcode = self.data.gread::<i8>(offset)? as bind_opcodes::Opcode;
            // let mut input = String::new();
            // ::std::io::stdin().read_line(&mut input).unwrap();
            // println!("opcode: {} ({:#x}) offset: {:#x}\n {:?}", opcode_to_str(opcode & BIND_OPCODE_MASK), opcode, *offset - location.start - 1, &bind_info);
            match opcode & BIND_OPCODE_MASK {
                // we do nothing, don't update our records, and add a new, fresh record
                BIND_OPCODE_DONE => {
                    bind_info = BindInformation::new(is_lazy);
                },
                BIND_OPCODE_SET_DYLIB_ORDINAL_IMM => {
	            let symbol_library_ordinal = opcode & BIND_IMMEDIATE_MASK;
	            bind_info.symbol_library_ordinal = symbol_library_ordinal;
                },
                BIND_OPCODE_SET_DYLIB_ORDINAL_ULEB => {
	            let symbol_library_ordinal = Uleb128::read(&self.data, offset)?;
	            bind_info.symbol_library_ordinal = symbol_library_ordinal as u8;
                },
                BIND_OPCODE_SET_DYLIB_SPECIAL_IMM => {
                    // dyld puts the immediate into the symbol_library_ordinal field...
                    let special_dylib = opcode & BIND_IMMEDIATE_MASK;
                    // Printf.printf "special_dylib: 0x%x\n" special_dylib
                    bind_info.special_dylib = special_dylib;
                },
                BIND_OPCODE_SET_SYMBOL_TRAILING_FLAGS_IMM => {
	            let symbol_flags = opcode & BIND_IMMEDIATE_MASK;
	            let symbol_name = self.data.pread::<&str>(*offset)?;
                    *offset = *offset + symbol_name.len() + 1; // second time this \0 caused debug woes
	            bind_info.symbol_name = symbol_name;
                    bind_info.symbol_flags = symbol_flags;
                },
                BIND_OPCODE_SET_TYPE_IMM => {
	            let bind_type = opcode & BIND_IMMEDIATE_MASK;
	            bind_info.bind_type = bind_type;
                },
                BIND_OPCODE_SET_ADDEND_SLEB => {
                    let addend = Sleb128::read(&self.data, offset)?;
                    bind_info.addend = addend;
                },
                BIND_OPCODE_SET_SEGMENT_AND_OFFSET_ULEB => {
	            let seg_index = opcode & BIND_IMMEDIATE_MASK;
                    // dyld sets the address to the segActualLoadAddress(segIndex) + uleb128
                    // address = segActualLoadAddress(segmentIndex) + read_uleb128(p, end);
	            let seg_offset = Uleb128::read(&self.data, offset)?;
	            bind_info.seg_index = seg_index;
                    bind_info.seg_offset = seg_offset;
                },
                BIND_OPCODE_ADD_ADDR_ULEB => {
	            let addr = Uleb128::read(&self.data, offset)?;
	            let seg_offset = bind_info.seg_offset.wrapping_add(addr);
	            bind_info.seg_offset = seg_offset;
                },
                // record the record by placing its value into our list
                BIND_OPCODE_DO_BIND => {
                    // from dyld:
                    //      if ( address >= segmentEndAddress ) 
	            // throwBadBindingAddress(address, segmentEndAddress, segmentIndex, start, end, p);
	            // (this->*handler)(context, address, type, symbolName, symboFlags, addend, libraryOrdinal, "", &last);
	            // address += sizeof(intptr_t);
                    let seg_offset = bind_info.seg_offset.wrapping_add(ctx.size() as u64);
                    bind_info.seg_offset = seg_offset;
                    imports.push(Import::new(&bind_info, libs, segments));
                },
                BIND_OPCODE_DO_BIND_ADD_ADDR_ULEB => {
                    // dyld:
	            // if ( address >= segmentEndAddress ) 
	            // throwBadBindingAddress(address, segmentEndAddress, segmentIndex, start, end, p);
	            // (this->*handler)(context, address, type, symbolName, symboFlags, addend, libraryOrdinal, "", &last);
	            // address += read_uleb128(p, end) + sizeof(intptr_t);
                    // we bind the old record, then increment bind info address for the next guy, plus the ptr offset *)
                    let addr = Uleb128::read(&self.data, offset)?;
                    let seg_offset = bind_info.seg_offset.wrapping_add(addr).wrapping_add(ctx.size() as u64);
                    bind_info.seg_offset = seg_offset;
                    imports.push(Import::new(&bind_info, libs, segments));
                },
                BIND_OPCODE_DO_BIND_ADD_ADDR_IMM_SCALED => {
                    // dyld:				
                    // if ( address >= segmentEndAddress ) 
	            // throwBadBindingAddress(address, segmentEndAddress, segmentIndex, start, end, p);
	            // (this->*handler)(context, address, type, symbolName, symboFlags, addend, libraryOrdinal, "", &last);
	            // address += immediate*sizeof(intptr_t) + sizeof(intptr_t);
	            // break;
                    // similarly, we bind the old record, then perform address manipulation for the next record
	            let scale = opcode & BIND_IMMEDIATE_MASK;
                    let size = ctx.size() as u64;
                    let seg_offset = bind_info.seg_offset.wrapping_add(scale as u64 * size).wrapping_add(size);
                    bind_info.seg_offset = seg_offset;
                    imports.push(Import::new(&bind_info, libs, segments));
                },
                BIND_OPCODE_DO_BIND_ULEB_TIMES_SKIPPING_ULEB => {
                    // dyld:
                    // count = read_uleb128(p, end);
	            // skip = read_uleb128(p, end);
	            // for (uint32_t i=0; i < count; ++i) {
	            // if ( address >= segmentEndAddress ) 
	            // throwBadBindingAddress(address, segmentEndAddress, segmentIndex, start, end, p);
	            // (this->*handler)(context, address, type, symbolName, symboFlags, addend, libraryOrdinal, "", &last);
	            // address += skip + sizeof(intptr_t);
	            // }
	            // break;
                    let count = Uleb128::read(&self.data, offset)?;
                    let skip =  Uleb128::read(&self.data, offset)?;
                    let mut addr = bind_info.seg_offset;
                    for _i  in 0..count {
                        addr += skip + ctx.size() as u64;
                    }
                    bind_info.seg_offset = addr;
                    imports.push(Import::new(&bind_info, libs, segments));
                },
                _ => {
                }
            }
        }        
        Ok(())
    }
}
