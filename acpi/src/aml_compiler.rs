// Copyright (c) 2020 Huawei Technologies Co.,Ltd. All rights reserved.
//
// StratoVirt is licensed under Mulan PSL v2.
// You can use this software according to the terms and conditions of the Mulan
// PSL v2.
// You may obtain a copy of Mulan PSL v2 at:
//         http://license.coscl.org.cn/MulanPSL2
// THIS SOFTWARE IS PROVIDED ON AN "AS IS" BASIS, WITHOUT WARRANTIES OF ANY
// KIND, EITHER EXPRESS OR IMPLIED, INCLUDING BUT NOT LIMITED TO
// NON-INFRINGEMENT, MERCHANTABILITY OR FIT FOR A PARTICULAR PURPOSE.
// See the Mulan PSL v2 for more details.

use util::byte_code::ByteCode;

const ACPI_NAME_SEG_MAX: u8 = 4;

/// This trait is used for converting AML Data structure to byte stream.
pub trait AmlBuilder {
    /// Transfer this struct to byte stream.
    fn aml_bytes(&self) -> Vec<u8>;
}

/// This trait is used for adding children to AML Data structure that represents
/// a scope, such as `AmlDevice`, `AmlScope`.
pub trait AmlScopeBuilder: AmlBuilder {
    /// Append a child to this AML scope structure.
    ///
    /// # Arguments
    ///
    /// * `child` - Child that will be appended to the end of this scope.
    fn append_child<T: AmlBuilder>(&mut self, child: T);
}

/// Macro that helps to define `AmlZero`, `AmlOne`, `AmlOnes`
///
/// # Arguments
///
/// * `$name` - struct name
/// * `$byte` - corresponding byte of this structure
macro_rules! zero_one_define {
    ($name: ident, $byte: expr) => {
        pub struct $name;

        impl AmlBuilder for $name {
            fn aml_bytes(&self) -> Vec<u8> {
                vec![$byte]
            }
        }
    };
}

zero_one_define!(AmlZero, 0x00);
zero_one_define!(AmlOne, 0x01);
zero_one_define!(AmlOnes, 0xFF);

/// Macro that helps to define `AmlByte`, `AmlWord`, `AmlDWord`, `AmlQWord`.
///
/// # Arguments
///
/// * `$name` - struct name
/// * `$op` - corresponding Opcode of this structure
/// * `$ty` - inner field of this struct.
macro_rules! aml_bytes_type_define {
    ($name:ident, $op:expr, $ty:tt) => {
        pub struct $name(pub $ty);

        impl AmlBuilder for $name {
            fn aml_bytes(&self) -> Vec<u8> {
                let mut bytes = Vec::new();
                bytes.push($op);
                bytes.extend(self.0.as_bytes());
                bytes
            }
        }
    };
}

aml_bytes_type_define!(AmlByte, 0x0A, u8);
aml_bytes_type_define!(AmlWord, 0x0B, u16);
aml_bytes_type_define!(AmlDWord, 0x0C, u32);
aml_bytes_type_define!(AmlQWord, 0x0E, u64);

/// Integer, max value u64::MAX.
pub struct AmlInteger(pub u64);

impl AmlBuilder for AmlInteger {
    fn aml_bytes(&self) -> Vec<u8> {
        match self.0 {
            0x00 => AmlZero.aml_bytes(),
            0x01 => AmlOne.aml_bytes(),
            0x02..=0xFF => AmlByte(self.0 as u8).aml_bytes(),
            0x100..=0xFFFF => AmlWord(self.0 as u16).aml_bytes(),
            0x10000..=0xFFFF_FFFF => AmlDWord(self.0 as u32).aml_bytes(),
            _ => AmlQWord(self.0).aml_bytes(),
        }
    }
}

/// String
pub struct AmlString(pub String);

impl AmlBuilder for AmlString {
    fn aml_bytes(&self) -> Vec<u8> {
        let mut bytes = Vec::new();
        bytes.push(0x0D);
        bytes.extend(self.0.as_bytes().to_vec());
        bytes.push(0x0);
        bytes
    }
}

/// Parse and check a name-segment, and convert it to byte stream.
fn build_name_seg(name: &str) -> Vec<u8> {
    if name.len() > usize::from(ACPI_NAME_SEG_MAX) {
        panic!("the length of NameSeg is larger than 4.");
    }

    let mut bytes = name.as_bytes().to_vec();
    bytes.extend(vec![b'_'; ACPI_NAME_SEG_MAX as usize - name.len()]);
    bytes
}

/// Parse a name-string and convert it to byte stream
fn build_name_string(name: &str) -> Vec<u8> {
    let strs = name.split('.').collect::<Vec<&str>>();
    if strs.is_empty() || strs.len() > 255 {
        panic!("Invalid ACPI name string, length: {}", strs.len());
    }

    let mut bytes = Vec::new();

    // parse the first segment.
    let mut first_str = strs[0].to_string();
    let mut index = 0;

    for (i, ch) in first_str.chars().enumerate() {
        if ch == '\\' || ch == '^' {
            bytes.push(ch as u8);
        } else {
            index = i;
            break;
        }
    }

    let remain_first = first_str.drain(index..).collect::<String>();

    match strs.len() {
        1 => {
            if remain_first.is_empty() {
                bytes.push(0x00);
            } else {
                bytes.append(&mut build_name_seg(&remain_first));
            }
        }
        2 => {
            bytes.push(0x2E);
            bytes.append(&mut build_name_seg(&remain_first));
            bytes.append(&mut build_name_seg(&strs[1].to_string()));
        }
        _ => {
            bytes.push(0x2F);
            bytes.push(strs.len() as u8);
            bytes.append(&mut build_name_seg(&remain_first));

            strs.iter().skip(1).for_each(|s| {
                bytes.extend(build_name_seg(&s.to_string()));
            })
        }
    }

    bytes
}

/// This struct represents declaration of a named object
pub struct AmlNameDecl {
    /// Name of the object.
    name: String,
    /// The corresponding object that be named.
    obj: Vec<u8>,
}

impl AmlNameDecl {
    pub fn new<T: AmlBuilder>(name: &str, obj: T) -> AmlNameDecl {
        AmlNameDecl {
            name: name.to_string(),
            obj: obj.aml_bytes(),
        }
    }
}

impl AmlBuilder for AmlNameDecl {
    fn aml_bytes(&self) -> Vec<u8> {
        let mut bytes = Vec::new();
        bytes.push(0x08);
        bytes.extend(build_name_string(self.name.as_ref()));
        bytes.extend(self.obj.clone());
        bytes
    }
}

/// AmlName represents a Named object that has been declared before.
#[derive(Clone)]
pub struct AmlName(pub String);

impl AmlBuilder for AmlName {
    fn aml_bytes(&self) -> Vec<u8> {
        build_name_string(self.0.as_ref())
    }
}

/// EISA ID String
pub struct AmlEisaId {
    name: String,
}

impl AmlEisaId {
    pub fn new(name: &str) -> AmlEisaId {
        if name.len() != 7 {
            panic!("Eisa: String length is not 7.");
        }
        AmlEisaId {
            name: name.to_string(),
        }
    }
}

impl AmlBuilder for AmlEisaId {
    fn aml_bytes(&self) -> Vec<u8> {
        let chars = self.name.chars().collect::<Vec<_>>();
        let dword: u32 = (chars[0] as u32 - 0x40) << 26
            | (chars[1] as u32 - 0x40) << 21
            | (chars[2] as u32 - 0x40) << 16
            | chars[3].to_digit(16).unwrap() << 12
            | chars[4].to_digit(16).unwrap() << 8
            | chars[5].to_digit(16).unwrap() << 4
            | chars[6].to_digit(16).unwrap();

        let mut bytes = dword.as_bytes().to_vec();
        bytes.reverse();
        bytes.insert(0, 0x0C);
        bytes
    }
}

/// Convert an ASCII string to a 128-bit buffer.
/// format: aabbccdd-eeff-gghh-iijj-kkllmmnnoopp
pub struct AmlToUuid {
    name: String,
}

impl AmlToUuid {
    pub fn new(str: &str) -> AmlToUuid {
        let name = str.to_string();
        if !Self::check_valid_uuid(&name) {
            panic!("Invalid UUID");
        }

        AmlToUuid { name }
    }

    /// Check if the uuid is valid.
    fn check_valid_uuid(uuid: &str) -> bool {
        if uuid.len() != 36 {
            return false;
        }

        // Char located at 8, 13, 18, 23 should be `-`
        let indexs = &[8, 13, 18, 23];
        for i in indexs {
            if uuid.chars().nth(*i).unwrap() != '-' {
                return false;
            }
        }

        for ch in uuid.chars() {
            if ch != '-' && (!ch.is_ascii_hexdigit()) {
                return false;
            }
        }

        true
    }
}

impl AmlBuilder for AmlToUuid {
    fn aml_bytes(&self) -> Vec<u8> {
        let mut bytes = Vec::new();

        // If the UUID is "aabbccdd-eeff-gghh-iijj-kkllmmnnoopp", then the encoded order is:
        // dd cc bb aa ff ee hh gg ii jj kk ll mm nn oo pp
        let index = &[6, 4, 2, 0, 11, 9, 16, 14, 19, 21, 24, 26, 28, 30, 32, 34];

        for i in index {
            let mut chars = self.name.chars();
            bytes.push(
                (chars.nth(*i).unwrap().to_digit(16).unwrap() as u8) << 4
                    | chars.next().unwrap().to_digit(16).unwrap() as u8,
            );
        }

        bytes
    }
}

// Follow ACPI spec: 5.4 Definition Block Encoding
// The lower two bits indicates how many bytes are used for PkgLength
// The 3,4 bits are only used if PkgLength consists of one bytes.
// Therefore, the max value of PkgLength is 0x3F(one-byte encoding),
// 0xF_FF(two-byte encoding), 0xF_FF_FF(three-byte encoding), 0xF_FF_FF_FF(four-byte encoding).
/// Calculate PkgLength according to the length, and convert it to bytes.
fn build_pkg_length(length: usize, include_self: bool) -> Vec<u8> {
    let pkg_1byte_shift = 6;
    let pkg_2byte_shift = 4;
    let pkg_3byte_shift = 12;
    let pkg_4byte_shift = 20;
    let mut pkg_length = length;
    let mut bytes = Vec::new();

    let bytes_count = if length + 1 < (1 << pkg_1byte_shift) {
        1
    } else if length + 2 < (1 << pkg_3byte_shift) {
        2
    } else if length + 3 < (1 << pkg_4byte_shift) {
        3
    } else {
        4
    };

    if include_self {
        pkg_length += bytes_count;
    }

    match bytes_count {
        1 => {
            bytes.push(pkg_length as u8);
        }
        2 => {
            bytes.push((1 << pkg_1byte_shift | (pkg_length & 0xF)) as u8);
            bytes.push((pkg_length >> pkg_2byte_shift) as u8);
        }
        3 => {
            bytes.push((2 << pkg_1byte_shift | (pkg_length & 0xF)) as u8);
            bytes.push((pkg_length >> pkg_2byte_shift) as u8);
            bytes.push((pkg_length >> pkg_3byte_shift) as u8);
        }
        4 => {
            bytes.push((3 << pkg_1byte_shift | (pkg_length & 0xF)) as u8);
            bytes.push((pkg_length >> pkg_2byte_shift) as u8);
            bytes.push((pkg_length >> pkg_3byte_shift) as u8);
            bytes.push((pkg_length >> pkg_4byte_shift) as u8);
        }
        _ => panic!("Undefined PkgLength"),
    }

    bytes
}

/// Buffer declaration, represents an array of bytes.
/// When a byte stream cannot by `AmlByte`, `AmlQWord`, etc, Buffer can be used.
pub struct AmlBuffer(pub Vec<u8>);

impl AmlBuilder for AmlBuffer {
    fn aml_bytes(&self) -> Vec<u8> {
        let mut bytes = Vec::new();
        bytes.push(0x11);
        let len_bytes = AmlInteger(self.0.len() as u64).aml_bytes();
        bytes.extend(build_pkg_length(len_bytes.len() + self.0.len(), true));
        bytes.extend(len_bytes);
        bytes.extend(self.0.clone());

        bytes
    }
}

/// Package contains an array of other objects.
pub struct AmlPackage {
    elem_count: u8,
    buf: Vec<u8>,
}

impl AmlPackage {
    pub fn new(elem_count: u8) -> AmlPackage {
        AmlPackage {
            elem_count,
            buf: vec![elem_count],
        }
    }
}

impl AmlBuilder for AmlPackage {
    fn aml_bytes(&self) -> Vec<u8> {
        let mut bytes = Vec::new();
        bytes.push(0x12);
        bytes.extend(build_pkg_length(self.buf.len(), true));
        bytes.extend(self.buf.clone());
        bytes
    }
}

impl AmlScopeBuilder for AmlPackage {
    fn append_child<T: AmlBuilder>(&mut self, child: T) {
        self.buf.extend(child.aml_bytes());
    }
}

/// Variable-sized Package.
pub struct AmlVarPackage {
    elem_count: u8,
    buf: Vec<u8>,
}

impl AmlVarPackage {
    pub fn new(elem_count: u8) -> AmlVarPackage {
        AmlVarPackage {
            elem_count,
            buf: vec![elem_count],
        }
    }
}

impl AmlBuilder for AmlVarPackage {
    fn aml_bytes(&self) -> Vec<u8> {
        let mut bytes = Vec::new();
        bytes.push(0x13);
        bytes.extend(build_pkg_length(self.buf.len(), true));
        bytes.extend(self.buf.clone());
        bytes
    }
}

impl AmlScopeBuilder for AmlVarPackage {
    fn append_child<T: AmlBuilder>(&mut self, child: T) {
        self.buf.extend(child.aml_bytes());
    }
}

/// Operation region address space type.
#[derive(Copy, Clone)]
pub enum AmlAddressSpaceType {
    /// System memory
    SystemMemory = 0,
    /// System IO
    SystemIO = 1,
    /// PCI config space
    PCIConfig = 2,
    /// Embedded controller space
    EmbeddedControl = 3,
    /// SMBus
    SMBus = 4,
    /// CMOS
    SystemCMOS = 5,
    /// PCI Bar target
    PciBarTarget = 6,
    /// IPMI
    IPMI = 7,
    /// General Purpose IO
    GeneralPurposeIO = 8,
    /// Generic serial bus
    GenericSerialBus = 9,
    /// Platform Communications Channel
    PCC = 10,
}

/// Operation Region: defines a named objects of certain type: SystemMemory, SystemIo, etc.
/// OperationRegion also contains the start address and size.
pub struct AmlOpRegion {
    /// Name of Operation region.
    name: String,
    /// System space type.
    space_type: AmlAddressSpaceType,
    /// Start address.
    offset: u64,
    /// Range length.
    length: u64,
}

impl AmlOpRegion {
    pub fn new(
        name: &str,
        space_type: AmlAddressSpaceType,
        offset: u64,
        length: u64,
    ) -> AmlOpRegion {
        AmlOpRegion {
            name: name.to_string(),
            space_type,
            offset,
            length,
        }
    }
}

impl AmlBuilder for AmlOpRegion {
    fn aml_bytes(&self) -> Vec<u8> {
        let mut bytes = Vec::new();
        bytes.push(0x5B);
        bytes.push(0x80);
        bytes.extend(build_name_string(self.name.as_ref()));
        bytes.push(self.space_type as u8);

        bytes.extend(AmlInteger(self.offset).aml_bytes());
        bytes.extend(AmlInteger(self.length).aml_bytes());
        bytes
    }
}

/// Access width of this field.
#[derive(Copy, Clone)]
pub enum AmlFieldAccessType {
    Any = 0,
    Byte = 1,
    Word = 2,
    DWord = 3,
    QWord = 4,
    Buffer = 5,
}

/// Flag that indicates whether the Global Lock is to be used
/// when accessing this field
#[derive(Copy, Clone)]
pub enum AmlFieldLockRule {
    NoLock = 0,
    Lock = 1,
}

/// Flag that indicates how the unmodified bits of a field are treated
#[derive(Copy, Clone)]
pub enum AmlFieldUpdateRule {
    Preserve = 0,
    WriteAsOnes = 1,
    WriteAsZeros = 2,
}

/// Field represents several bits in Operation Field.
pub struct AmlField {
    /// The name of corresponding OperationRegion.
    name: String,
    /// The access type of this Field.
    access_type: AmlFieldAccessType,
    /// Global lock is to be used or not when accessing this field.
    lock_rule: AmlFieldLockRule,
    /// Unmodified bits of a field are treated as Ones/Zeros/Preserve.
    update_rule: AmlFieldUpdateRule,
    /// Field Unit list.
    buf: Vec<u8>,
}

impl AmlField {
    pub fn new(
        name: &str,
        acc_ty: AmlFieldAccessType,
        lock_r: AmlFieldLockRule,
        update_r: AmlFieldUpdateRule,
    ) -> AmlField {
        let mut bytes = Vec::new();
        let flag = ((update_r as u8) << 5) | ((lock_r as u8) << 1) | (acc_ty as u8 & 0x0F);
        bytes.extend(build_name_string(name));
        bytes.push(flag);

        AmlField {
            name: name.to_string(),
            access_type: acc_ty,
            lock_rule: lock_r,
            update_rule: update_r,
            buf: bytes,
        }
    }
}

impl AmlBuilder for AmlField {
    fn aml_bytes(&self) -> Vec<u8> {
        let mut bytes = Vec::new();
        bytes.push(0x5B);
        bytes.push(0x81);
        bytes.extend(build_pkg_length(self.buf.len(), true));
        bytes.extend(self.buf.clone());
        bytes
    }
}

impl AmlScopeBuilder for AmlField {
    fn append_child<T: AmlBuilder>(&mut self, child: T) {
        self.buf.extend(child.aml_bytes());
    }
}

/// Field unit that defines inside Field-scope.
pub struct AmlFieldUnit {
    /// The name of this Field unit. `None` value indicate that this field is reserved.
    name: Option<String>,
    /// The Byte length of this Field unit.
    length: u8,
}

impl AmlFieldUnit {
    pub fn new(name: Option<&str>, length: u8) -> AmlFieldUnit {
        AmlFieldUnit {
            name: name.map(|s| s.to_string()),
            length,
        }
    }
}

impl AmlBuilder for AmlFieldUnit {
    fn aml_bytes(&self) -> Vec<u8> {
        let mut bytes = self
            .name
            .as_ref()
            .map(|str| build_name_seg(&str))
            .unwrap_or_else(|| vec![0x0_u8]);
        bytes.extend(build_pkg_length(self.length as usize, false));
        bytes
    }
}

/// Open a named Scope, can refer any scope within the namespace.
pub struct AmlScope {
    /// The name of scope.
    name: String,
    /// Contains objects created inside the scope, which are encodes to bytes.
    buf: Vec<u8>,
}

impl AmlScope {
    pub fn new(name: &str) -> AmlScope {
        AmlScope {
            name: name.to_string(),
            buf: build_name_string(name),
        }
    }
}

impl AmlBuilder for AmlScope {
    fn aml_bytes(&self) -> Vec<u8> {
        let mut bytes = Vec::new();
        bytes.push(0x10);
        bytes.extend(build_pkg_length(self.buf.len(), true));
        bytes.extend(self.buf.clone());

        bytes
    }
}

impl AmlScopeBuilder for AmlScope {
    fn append_child<T: AmlBuilder>(&mut self, child: T) {
        self.buf.extend(child.aml_bytes());
    }
}

/// Device object that represents a processor, a device, etc.
pub struct AmlDevice {
    name: String,
    buf: Vec<u8>,
}

impl AmlDevice {
    pub fn new(name: &str) -> AmlDevice {
        AmlDevice {
            name: name.to_string(),
            buf: build_name_string(name),
        }
    }
}

impl AmlBuilder for AmlDevice {
    fn aml_bytes(&self) -> Vec<u8> {
        let mut bytes = Vec::new();
        bytes.push(0x5B);
        bytes.push(0x82);
        bytes.extend(build_pkg_length(self.buf.len(), true));
        bytes.extend(self.buf.clone());
        bytes
    }
}

impl AmlScopeBuilder for AmlDevice {
    fn append_child<T: AmlBuilder>(&mut self, child: T) {
        self.buf.extend(child.aml_bytes());
    }
}

/// Method defination.
pub struct AmlMethod {
    /// The name of this method.
    name: String,
    /// Count of Arguments. default value is zero.
    args_count: u8,
    /// Whether this method is Serialized or not.
    serialized: bool,
    /// The body of this method, which has been converted to byte stream.
    buf: Vec<u8>,
}

impl AmlMethod {
    pub fn new(name: &str, args_count: u8, serialized: bool) -> AmlMethod {
        if args_count > 7 {
            panic!("Up to 7 arguments are supported, given {}", args_count);
        }

        // Method Flags:
        //  bit 0-2: ArgCount (0-7)
        //  bit 3: SerializeFlag
        //      0 NotSerialized
        //      1 Serialized
        let mut flag = args_count;
        if serialized {
            flag |= 1 << 3;
        }

        let mut bytes = build_name_string(name);
        bytes.push(flag);

        AmlMethod {
            name: name.to_string(),
            args_count,
            serialized,
            buf: bytes,
        }
    }
}

impl AmlBuilder for AmlMethod {
    fn aml_bytes(&self) -> Vec<u8> {
        let mut bytes = Vec::new();
        bytes.push(0x14);
        bytes.extend(build_pkg_length(self.buf.len(), true));
        bytes.extend(self.buf.clone());
        bytes
    }
}

impl AmlScopeBuilder for AmlMethod {
    fn append_child<T: AmlBuilder>(&mut self, child: T) {
        self.buf.extend(child.aml_bytes());
    }
}

/// Local variables that can be used in method, Local(0)~Local(7)
#[derive(Copy, Clone)]
pub struct AmlLocal(pub u8);

impl AmlBuilder for AmlLocal {
    fn aml_bytes(&self) -> Vec<u8> {
        if self.0 > 7 {
            panic!("Up to 8 Local are supported, given {}", self.0);
        }
        vec![0x60 + self.0]
    }
}

/// Arguments passed to method, Arg(0)~Arg(6)
#[derive(Copy, Clone)]
pub struct AmlArg(pub u8);

impl AmlBuilder for AmlArg {
    fn aml_bytes(&self) -> Vec<u8> {
        if self.0 > 6 {
            panic!("Up to 7 Arguments are supported, given {}", self.0);
        }
        vec![0x68 + self.0]
    }
}

/// Return from method
pub struct AmlReturn {
    value: Vec<u8>,
}

impl AmlReturn {
    /// Return with nothing.
    #[allow(clippy::new_without_default)]
    pub fn new() -> AmlReturn {
        AmlReturn { value: vec![0x00] }
    }

    /// Return an object or reference.
    pub fn with_value<T: AmlBuilder>(val: T) -> AmlReturn {
        AmlReturn {
            value: val.aml_bytes(),
        }
    }
}

impl AmlBuilder for AmlReturn {
    fn aml_bytes(&self) -> Vec<u8> {
        let mut bytes = Vec::new();
        bytes.push(0xA4);
        bytes.extend(self.value.clone());

        bytes
    }
}

/// Call a method.
pub struct AmlCall {
    /// Name of the method that will be called.
    name: String,
    /// The arguments that will be passed to the method. Note that
    /// the arguments provided must match to the arguments' count of the method.
    buf: Vec<u8>,
}

impl AmlCall {
    pub fn new<T: AmlBuilder>(name: &str, args: Vec<T>) -> AmlCall {
        let mut bytes = Vec::new();
        args.iter().for_each(|arg| bytes.extend(arg.aml_bytes()));

        AmlCall {
            name: name.to_string(),
            buf: bytes,
        }
    }
}

impl AmlBuilder for AmlCall {
    fn aml_bytes(&self) -> Vec<u8> {
        let mut bytes = Vec::new();
        bytes.extend(build_name_string(&self.name));
        bytes.extend(self.buf.clone());

        bytes
    }
}

/// Macro that helps to define Aml Data structures that
/// contains two arguments and one destination.
macro_rules! ops_2args_dst_define {
    ($name:ident, $op:expr) => {
        pub struct $name {
            arg1: Vec<u8>,
            arg2: Vec<u8>,
            dst: Vec<u8>,
        }

        impl $name {
            pub fn new<A: AmlBuilder, B: AmlBuilder, D: AmlBuilder>(a1: A, a2: B, d: D) -> $name {
                $name {
                    arg1: a1.aml_bytes(),
                    arg2: a2.aml_bytes(),
                    dst: d.aml_bytes(),
                }
            }
        }

        impl AmlBuilder for $name {
            fn aml_bytes(&self) -> Vec<u8> {
                let mut bytes = Vec::new();
                bytes.push($op);
                bytes.extend(self.arg1.clone());
                bytes.extend(self.arg2.clone());
                bytes.extend(self.dst.clone());

                bytes
            }
        }
    };
}

ops_2args_dst_define!(AmlAdd, 0x72);
ops_2args_dst_define!(AmlSubtract, 0x74);
// Concatenate two strings, integers or buffers and store the result in Destination.
ops_2args_dst_define!(AmlConcat, 0x73);
ops_2args_dst_define!(AmlAnd, 0x7B);
ops_2args_dst_define!(AmlOr, 0x7D);
ops_2args_dst_define!(AmlShiftLeft, 0x79);
ops_2args_dst_define!(AmlShiftRight, 0x7A);
// Indexed Reference to member object.
// The first argument refers to the source that can be Buffer, Package or String.
// The second argument refers to the index.
// The corresponding reference to the field is stored in Destination.
ops_2args_dst_define!(AmlIndex, 0x88);

/// Macro that helps to define Aml Data structures that contains one argument.
macro_rules! ops_1arg_define {
    ($name:ident, $op:expr) => {
        pub struct $name {
            arg: Vec<u8>,
        }

        impl $name {
            pub fn new<T: AmlBuilder>(a1: T) -> $name {
                $name {
                    arg: a1.aml_bytes(),
                }
            }
        }

        impl AmlBuilder for $name {
            fn aml_bytes(&self) -> Vec<u8> {
                let mut bytes = Vec::new();
                bytes.push($op);
                bytes.extend(self.arg.clone());

                bytes
            }
        }
    };
}

// Increment/Decrement: Arg1 is an Integer.
ops_1arg_define!(AmlIncrement, 0x75);
ops_1arg_define!(AmlDecrement, 0x76);
// Logical Not: Arg1 is an Integer.
ops_1arg_define!(AmlLNot, 0x92);
// SizeOf: Arg1 must be an Buffer, String or Package.
ops_1arg_define!(AmlSizeOf, 0x87);

// DeRefOf is necessary in the first operand of the Store operator
// in order to get the actual object, rather than just a reference to the object.
// If DeRefOf were not used, then `Index` would contain an object reference to the element.
// Below is an example:
//
// ```
// let buffer = AmlBuffer::new(vec![0x1, 0x2, 0x3]);
// let arg1 = AmlDeRefOf::new(AmlIndex::new(buffer, 1));
// let store = AmlStore::new(arg1, Local(0));
// ```
ops_1arg_define!(AmlDeRefOf, 0x83);

/// Macro that helps to define Aml Data structures that contains two arguments.
macro_rules! ops_2arg_define {
    ($name:ident, $op:expr) => {
        pub struct $name {
            arg1: Vec<u8>,
            arg2: Vec<u8>,
        }

        impl $name {
            pub fn new<A: AmlBuilder, B: AmlBuilder>(a1: A, a2: B) -> $name {
                $name {
                    arg1: a1.aml_bytes(),
                    arg2: a2.aml_bytes(),
                }
            }
        }

        impl AmlBuilder for $name {
            fn aml_bytes(&self) -> Vec<u8> {
                let mut bytes = Vec::new();
                bytes.push($op);
                bytes.extend(self.arg1.clone());
                bytes.extend(self.arg2.clone());

                bytes
            }
        }
    };
}

// Store the source value(Arg1) to the destination(Arg2).
ops_2arg_define!(AmlStore, 0x70);
// Notify the Object(Arg1) that the NotificationValue(Arg2) has occurred.
ops_2arg_define!(AmlNotify, 0x86);

// Logical Equal/Greater/Less/And/Or:
// Arg1 and Arg2 must be the same type. Integer, String and Buffer are supported.
// Return True or False;
ops_2arg_define!(AmlEqual, 0x93);
ops_2arg_define!(AmlLGreater, 0x94);
ops_2arg_define!(AmlLLess, 0x95);
ops_2arg_define!(AmlLAnd, 0x90);
ops_2arg_define!(AmlLOr, 0x91);

/// If scope
pub struct AmlIf {
    /// Predicate and the operations in the if-scope is converted to byte stream.
    buf: Vec<u8>,
}

impl AmlIf {
    pub fn new<T: AmlBuilder>(predicate: T) -> AmlIf {
        AmlIf {
            buf: predicate.aml_bytes(),
        }
    }
}

impl AmlBuilder for AmlIf {
    fn aml_bytes(&self) -> Vec<u8> {
        let mut bytes = Vec::new();
        bytes.push(0xA0);
        bytes.extend(build_pkg_length(self.buf.len(), true));
        bytes.extend(self.buf.clone());

        bytes
    }
}

impl AmlScopeBuilder for AmlIf {
    fn append_child<T: AmlBuilder>(&mut self, child: T) {
        self.buf.extend(child.aml_bytes());
    }
}

/// Else scope
pub struct AmlElse {
    /// Predicate and the operations in the else-scope is converted to byte stream.
    buf: Vec<u8>,
}

impl AmlElse {
    #[allow(clippy::new_without_default)]
    pub fn new() -> AmlElse {
        AmlElse { buf: Vec::new() }
    }
}

impl AmlBuilder for AmlElse {
    fn aml_bytes(&self) -> Vec<u8> {
        let mut bytes = Vec::new();
        bytes.push(0xA1);
        bytes.extend(build_pkg_length(self.buf.len(), true));
        bytes.extend(self.buf.clone());

        bytes
    }
}

impl AmlScopeBuilder for AmlElse {
    fn append_child<T: AmlBuilder>(&mut self, child: T) {
        self.buf.extend(child.aml_bytes());
    }
}

/// While scope
pub struct AmlWhile {
    /// Predicate and the operations in the while-scope is converted to byte stream.
    buf: Vec<u8>,
}

impl AmlWhile {
    pub fn new<T: AmlBuilder>(predicate: T) -> AmlWhile {
        AmlWhile {
            buf: predicate.aml_bytes(),
        }
    }
}

impl AmlBuilder for AmlWhile {
    fn aml_bytes(&self) -> Vec<u8> {
        let mut bytes = Vec::new();
        bytes.push(0xA2);
        bytes.extend(build_pkg_length(self.buf.len(), true));
        bytes.extend(self.buf.clone());

        bytes
    }
}

impl AmlScopeBuilder for AmlWhile {
    fn append_child<T: AmlBuilder>(&mut self, child: T) {
        self.buf.extend(child.aml_bytes());
    }
}

#[cfg(test)]
mod test {
    use super::*;

    #[test]
    fn test_uuid() {
        let uuid = AmlToUuid::new("33DB4D5B-1FF7-401C-9657-7441C03DD766");

        assert_eq!(
            uuid.aml_bytes(),
            &[
                0x5B, 0x4D, 0xDB, 0x33, 0xF7, 0x1F, 0x1C, 0x40, 0x96, 0x57, 0x74, 0x41, 0xC0, 0x3D,
                0xD7, 0x66
            ]
        );
    }
    #[test]
    fn test_package() {
        // Name (PKG1, Package(3){0x1234, "Hello world", INT1})
        let mut pkg1 = AmlPackage::new(3);
        pkg1.append_child(AmlInteger(0x1234));
        pkg1.append_child(AmlString("Hello world".to_string()));
        pkg1.append_child(AmlName("INT1".to_string()));
        let named_pkg1 = AmlNameDecl::new("PKG1", pkg1);

        let pkg1_bytes = vec![
            0x08, 0x50, 0x4B, 0x47, 0x31, 0x12, 0x16, 0x03, 0x0B, 0x34, 0x12, 0x0D, 0x48, 0x65,
            0x6C, 0x6C, 0x6F, 0x20, 0x77, 0x6F, 0x72, 0x6C, 0x64, 0x00, 0x49, 0x4E, 0x54, 0x31,
        ];
        assert_eq!(named_pkg1.aml_bytes(), pkg1_bytes);

        // Name (PKG2, Package(){INT1, "Good bye"})
        let mut pkg2 = AmlPackage::new(2);
        pkg2.append_child(AmlName("INT1".to_string()));
        pkg2.append_child(AmlString("Good bye".to_string()));
        let named_pkg2 = AmlNameDecl::new("PKG2", pkg2);

        let pkg2_bytes = vec![
            0x08, 0x50, 0x4B, 0x47, 0x32, 0x12, 0x10, 0x02, 0x49, 0x4E, 0x54, 0x31, 0x0D, 0x47,
            0x6F, 0x6F, 0x64, 0x20, 0x62, 0x79, 0x65, 0x00,
        ];
        assert_eq!(named_pkg2.aml_bytes(), pkg2_bytes);

        // Name (PKG3, Package(){
        //     "ASL is fun",
        //     Package() {0xff, 0xfe, 0xfd},
        //     Buffer() {0x01, 0x02}
        // })
        let mut pkg3 = AmlPackage::new(3);
        pkg3.append_child(AmlString("ASL is fun".to_string()));

        let mut pkg32 = AmlPackage::new(3);
        pkg32.append_child(AmlInteger(0xff));
        pkg32.append_child(AmlInteger(0xfe));
        pkg32.append_child(AmlInteger(0xfd));
        pkg3.append_child(pkg32);

        let buffer = AmlBuffer(vec![0x01, 0x02]);
        pkg3.append_child(buffer);
        let named_pkg3 = AmlNameDecl::new("PKG3", pkg3);

        let pkg3_bytes = vec![
            0x08, 0x50, 0x4B, 0x47, 0x33, 0x12, 0x1D, 0x03, 0x0D, 0x41, 0x53, 0x4C, 0x20, 0x69,
            0x73, 0x20, 0x66, 0x75, 0x6E, 0x00, 0x12, 0x08, 0x03, 0x0A, 0xFF, 0x0A, 0xFE, 0x0A,
            0xFD, 0x11, 0x05, 0x0A, 0x02, 0x01, 0x02,
        ];
        assert_eq!(named_pkg3.aml_bytes(), pkg3_bytes);
    }

    #[test]
    fn test_op_region() {
        // OperationRegion(OPR1, SystemMemory, 0x10000, 0x5)
        // Field (OPR1, ByteAcc, NoLock, WriteAsZeros)
        // {
        //     FLD1, 8,
        //     FLD2, 8,
        //     Offset (3), //Start the next field unit at byte offset 3
        //     FLD3, 4,
        //     FLD4, 12,
        // }
        let op_region = AmlOpRegion::new("OPR1", AmlAddressSpaceType::SystemMemory, 0x10000, 0x5);
        let target = vec![
            0x5B, 0x80, 0x4F, 0x50, 0x52, 0x31, 0x00, 0x0C, 0x00, 0x00, 0x01, 0x00, 0x0A, 0x05,
        ];
        assert_eq!(op_region.aml_bytes(), target);

        let mut field = AmlField::new(
            "OPR1",
            AmlFieldAccessType::Byte,
            AmlFieldLockRule::NoLock,
            AmlFieldUpdateRule::WriteAsZeros,
        );

        let elem1 = AmlFieldUnit::new(Some("FLD1"), 8);
        let elem2 = AmlFieldUnit::new(Some("FLD2"), 8);
        let elem3 = AmlFieldUnit::new(None, 8);
        let elem4 = AmlFieldUnit::new(Some("FLD3"), 4);
        let elem5 = AmlFieldUnit::new(Some("FLD4"), 12);

        for e in vec![elem1, elem2, elem3, elem4, elem5] {
            field.append_child(e);
        }

        let target = vec![
            0x5B, 0x81, 0x1C, 0x4F, 0x50, 0x52, 0x31, 0x41, 0x46, 0x4C, 0x44, 0x31, 0x08, 0x46,
            0x4C, 0x44, 0x32, 0x08, 0x00, 0x08, 0x46, 0x4C, 0x44, 0x33, 0x04, 0x46, 0x4C, 0x44,
            0x34, 0x0C,
        ];
        assert_eq!(field.aml_bytes(), target);
    }

    #[test]
    fn test_device() {
        // Device (PCI0)
        // {
        //     Name (_HID, EisaId ("PNP0A03"))
        // }
        let mut device = AmlDevice::new("PCI0");
        let hid = AmlNameDecl::new("_HID", AmlEisaId::new("PNP0A03"));
        device.append_child(hid);

        let bytes = device.aml_bytes();
        let target = vec![
            0x5B, 0x82, 0x0F, 0x50, 0x43, 0x49, 0x30, 0x08, 0x5F, 0x48, 0x49, 0x44, 0x0C, 0x41,
            0xD0, 0x0A, 0x03,
        ];
        assert_eq!(bytes, target);
    }

    #[test]
    fn test_scope() {
        // Scope (_SB) {
        //     Name (INT1, 0xABCD)
        // }
        let mut scope = AmlScope::new("_SB");
        scope.append_child(AmlNameDecl::new("INT1", AmlInteger(0xABCD)));

        let target = vec![
            0x10, 0x0D, 0x5F, 0x53, 0x42, 0x5F, 0x08, 0x49, 0x4E, 0x54, 0x31, 0x0B, 0xCD, 0xAB,
        ];

        assert_eq!(scope.aml_bytes(), target);
    }

    #[test]
    fn test_method() {
        // Scope(\_SB.PCI4) {
        //     OperationRegion(LED1, SystemIO, 0x10C0, 0x20)
        //     Field(LED1, AnyAcc, NoLock, Preserve)
        //     { // LED controls
        //         S0LE, 1, // Slot 0 Ejection Progress LED
        //         S0LF, 1, // Slot 0 Ejection Failure LED
        //         S1LE, 1, // Slot 1 Ejection Progress LED
        //         S1LF, 1, // Slot 1 Ejection Failure LED
        //         S2LE, 1, // Slot 2 Ejection Progress LED
        //         S2LF, 1, // Slot 2 Ejection Failure LED
        //         S3LE, 1, // Slot 3 Ejection Progress LED
        //         S3LF, 1 // Slot 3 Ejection Failure LED
        //     }
        //     Device(SLT3) { // hot plug device
        //         Name(_ADR, 0x000C0003)
        //         Method(_OST, 3, Serialized) {
        //             If(LEqual(Arg0,Ones)) // Unspecified event
        //             {
        //                 Store(Zero, Arg1)
        //             }
        //             Store(Zero, Arg2) // Turn off Ejection Progress LED
        //             Store(One, Arg0) // Turn on Ejection Failure LED
        //         }
        //     }
        // }
        let mut scope1 = AmlScope::new("\\_SB.PCI4");

        let op_region = AmlOpRegion::new("LED1", AmlAddressSpaceType::SystemIO, 0x10C0, 0x20);
        let mut field = AmlField::new(
            "LED1",
            AmlFieldAccessType::Any,
            AmlFieldLockRule::NoLock,
            AmlFieldUpdateRule::Preserve,
        );
        let mut elems = Vec::new();
        elems.push(AmlFieldUnit::new(Some("S0LE"), 1));
        elems.push(AmlFieldUnit::new(Some("S0LF"), 1));
        elems.push(AmlFieldUnit::new(Some("S1LE"), 1));
        elems.push(AmlFieldUnit::new(Some("S1LF"), 1));
        elems.push(AmlFieldUnit::new(Some("S2LE"), 1));
        elems.push(AmlFieldUnit::new(Some("S2LF"), 1));
        elems.push(AmlFieldUnit::new(Some("S3LE"), 1));
        elems.push(AmlFieldUnit::new(Some("S3LF"), 1));
        for e in elems {
            field.append_child(e);
        }

        let mut device = AmlDevice::new("SLT3");

        let name1 = AmlNameDecl::new("_ADR", AmlInteger(0x000C0003));

        let mut method1 = AmlMethod::new("_OST", 3, true);
        let mut if_scope = AmlIf::new(AmlEqual::new(AmlArg(0), AmlOnes));
        let store1 = AmlStore::new(AmlZero, AmlArg(1));
        if_scope.append_child(store1);
        let store2 = AmlStore::new(AmlZero, AmlArg(2));
        let store3 = AmlStore::new(AmlOne, AmlArg(0));
        method1.append_child(if_scope);
        method1.append_child(store2);
        method1.append_child(store3);

        device.append_child(name1);
        device.append_child(method1);
        scope1.append_child(op_region);
        scope1.append_child(field);
        scope1.append_child(device);

        let scope1_bytes = vec![
            0x10, 0x4E, 0x06, 0x5C, 0x2E, 0x5F, 0x53, 0x42, 0x5F, 0x50, 0x43, 0x49, 0x34, 0x5B,
            0x80, 0x4C, 0x45, 0x44, 0x31, 0x01, 0x0B, 0xC0, 0x10, 0x0A, 0x20, 0x5B, 0x81, 0x2E,
            0x4C, 0x45, 0x44, 0x31, 0x00, 0x53, 0x30, 0x4C, 0x45, 0x01, 0x53, 0x30, 0x4C, 0x46,
            0x01, 0x53, 0x31, 0x4C, 0x45, 0x01, 0x53, 0x31, 0x4C, 0x46, 0x01, 0x53, 0x32, 0x4C,
            0x45, 0x01, 0x53, 0x32, 0x4C, 0x46, 0x01, 0x53, 0x33, 0x4C, 0x45, 0x01, 0x53, 0x33,
            0x4C, 0x46, 0x01, 0x5B, 0x82, 0x24, 0x53, 0x4C, 0x54, 0x33, 0x08, 0x5F, 0x41, 0x44,
            0x52, 0x0C, 0x03, 0x00, 0x0C, 0x00, 0x14, 0x14, 0x5F, 0x4F, 0x53, 0x54, 0x0B, 0xA0,
            0x07, 0x93, 0x68, 0xFF, 0x70, 0x00, 0x69, 0x70, 0x00, 0x6A, 0x70, 0x01, 0x68,
        ];
        assert_eq!(scope1.aml_bytes(), scope1_bytes);

        // Scope (\_GPE)
        // {
        //     Method(_E13)
        //     {
        //         Store(One, \_SB.PCI4.S3LE) // Turn on ejection request LED
        //         Notify(\_SB.PCI4.SLT3, 3) // Ejection request driven from GPE13
        //     }
        // }
        let mut scope2 = AmlScope::new("\\_GPE");
        let mut method2 = AmlMethod::new("_E13", 0, false);
        let store4 = AmlStore::new(AmlOne, AmlName("\\_SB.PCI4.S3LE".to_string()));
        let notify = AmlNotify::new(AmlName("\\_SB.PCI4.SLT3".to_string()), AmlInteger(3));
        method2.append_child(store4);
        method2.append_child(notify);
        scope2.append_child(method2);

        let scope2_bytes = vec![
            0x10, 0x30, 0x5C, 0x5F, 0x47, 0x50, 0x45, 0x14, 0x29, 0x5F, 0x45, 0x31, 0x33, 0x00,
            0x70, 0x01, 0x5C, 0x2F, 0x03, 0x5F, 0x53, 0x42, 0x5F, 0x50, 0x43, 0x49, 0x34, 0x53,
            0x33, 0x4C, 0x45, 0x86, 0x5C, 0x2F, 0x03, 0x5F, 0x53, 0x42, 0x5F, 0x50, 0x43, 0x49,
            0x34, 0x53, 0x4C, 0x54, 0x33, 0x0A, 0x03,
        ];
        assert_eq!(scope2.aml_bytes(), scope2_bytes);
    }

    #[test]
    fn test_arithmetic_ops() {
        // Method(INCR, 3) {
        //     If (Arg1 == Arg2) {
        //         Return
        //     }
        //     Local0 = Arg0
        //     While (Local0 < Arg1) {
        //         Local0++;
        //     }
        //     Local0--;
        //     Local0 += 2;
        //     If (Local0 > Arg2) {
        //         Local0 -= Arg2;
        //     }
        // }
        let mut method1 = AmlMethod::new("INCR", 3, false);
        let mut if_scope1 = AmlIf::new(AmlEqual::new(AmlArg(1), AmlArg(2)));
        if_scope1.append_child(AmlReturn::new());
        method1.append_child(if_scope1);

        let store1 = AmlStore::new(AmlArg(0), AmlLocal(0).clone());
        method1.append_child(store1);

        let mut while_scope = AmlWhile::new(AmlLLess::new(AmlLocal(0), AmlArg(1)));
        while_scope.append_child(AmlIncrement::new(AmlLocal(0)));
        method1.append_child(while_scope);

        method1.append_child(AmlDecrement::new(AmlLocal(0)));
        method1.append_child(AmlAdd::new(AmlLocal(0), AmlInteger(2), AmlLocal(0)));

        let mut if_scope2 = AmlIf::new(AmlLGreater::new(AmlLocal(0), AmlArg(2)));
        if_scope2.append_child(AmlSubtract::new(AmlLocal(0), AmlArg(2), AmlLocal(0)));
        method1.append_child(if_scope2);

        let method1_bytes = vec![
            0x14, 0x27, 0x49, 0x4E, 0x43, 0x52, 0x03, 0xA0, 0x06, 0x93, 0x69, 0x6A, 0xA4, 0x00,
            0x70, 0x68, 0x60, 0xA2, 0x06, 0x95, 0x60, 0x69, 0x75, 0x60, 0x76, 0x60, 0x72, 0x60,
            0x0A, 0x02, 0x60, 0xA0, 0x08, 0x94, 0x60, 0x6A, 0x74, 0x60, 0x6A, 0x60,
        ];
        assert_eq!(method1.aml_bytes(), method1_bytes);

        // Method(MTD2, 1) {
        //     Local0 = SizeOf(Arg0)
        //     Name (PKG1, Package () {
        //         0x01, 0x03F8, 0x03FF
        //     })
        //
        //     Name (PKG2, Package(2){0x1234, "Hello world"})
        //
        //     Store (DeRefOf (Index (PKG1, 5)), Local0)
        //     Concatenate(PKG1, PKG2, Local1)
        //
        //     Return(Local0)
        // }
        let mut method2 = AmlMethod::new("MTD2", 1, false);

        let store2 = AmlStore::new(AmlSizeOf::new(AmlArg(0)), AmlLocal(0));
        method2.append_child(store2);

        let mut pkg1 = AmlPackage::new(3);
        vec![0x01, 0x03F8, 0x03FF].iter().for_each(|&x| {
            pkg1.append_child(AmlInteger(x as u64));
        });
        let named_pkg1 = AmlNameDecl::new("PKG1", pkg1);
        method2.append_child(named_pkg1);

        let mut pkg2 = AmlPackage::new(2);
        pkg2.append_child(AmlInteger(0x1234));
        pkg2.append_child(AmlString("Hello world".to_string()));
        let named_pkg2 = AmlNameDecl::new("PKG2", pkg2);
        method2.append_child(named_pkg2);

        let pkg1_str = AmlName("PKG1".to_string());
        let pkg2_str = AmlName("PKG2".to_string());
        let store3 = AmlStore::new(
            AmlDeRefOf::new(AmlIndex::new(pkg1_str.clone(), AmlInteger(5), AmlZero)),
            AmlLocal(0),
        );
        let concat = AmlConcat::new(pkg1_str, pkg2_str, AmlLocal(1));
        method2.append_child(store3);
        method2.append_child(concat);

        let return_value = AmlReturn::with_value(AmlLocal(0));
        method2.append_child(return_value);

        let method2_bytes = vec![
            0x14, 0x49, 0x04, 0x4D, 0x54, 0x44, 0x32, 0x01, 0x70, 0x87, 0x68, 0x60, 0x08, 0x50,
            0x4B, 0x47, 0x31, 0x12, 0x09, 0x03, 0x01, 0x0B, 0xF8, 0x03, 0x0B, 0xFF, 0x03, 0x08,
            0x50, 0x4B, 0x47, 0x32, 0x12, 0x12, 0x02, 0x0B, 0x34, 0x12, 0x0D, 0x48, 0x65, 0x6C,
            0x6C, 0x6F, 0x20, 0x77, 0x6F, 0x72, 0x6C, 0x64, 0x00, 0x70, 0x83, 0x88, 0x50, 0x4B,
            0x47, 0x31, 0x0A, 0x05, 0x00, 0x60, 0x73, 0x50, 0x4B, 0x47, 0x31, 0x50, 0x4B, 0x47,
            0x32, 0x61, 0xA4, 0x60,
        ];
        assert_eq!(method2.aml_bytes(), method2_bytes);
    }

    #[test]
    fn test_logical_ops() {
        // Method(MTD3, 1) {
        //     Local0 = Arg0
        //
        //     Local1 = (Local0 << 3) | 0xFF
        //     Local2 = (Local0 >> 3) & 0xFF
        //
        //     If (Local1 && Local2) {
        //         Return(2)
        //     }
        //
        //     if (Local1 || Local2) {
        //         Return(1)
        //     }
        //
        //     Return(0)
        // }
        let mut method = AmlMethod::new("MTD3", 1, false);

        let store1 = AmlStore::new(AmlArg(0), AmlLocal(0));
        method.append_child(store1);

        let shift_left = AmlShiftLeft::new(AmlLocal(0), AmlInteger(0x3), AmlZero);
        let store2 = AmlOr::new(shift_left, AmlInteger(0xFF), AmlLocal(1));
        method.append_child(store2);

        let shift_right = AmlShiftRight::new(AmlLocal(0), AmlInteger(0x3), AmlZero);
        let store3 = AmlAnd::new(shift_right, AmlInteger(0xFF), AmlLocal(2));
        method.append_child(store3);

        let mut if_scope1 = AmlIf::new(AmlLAnd::new(AmlLocal(1), AmlLocal(2)));
        if_scope1.append_child(AmlReturn::with_value(AmlInteger(2)));
        method.append_child(if_scope1);

        let mut if_scope2 = AmlIf::new(AmlLOr::new(AmlLocal(1), AmlLocal(2)));
        if_scope2.append_child(AmlReturn::with_value(AmlInteger(1)));
        method.append_child(if_scope2);

        method.append_child(AmlReturn::with_value(AmlInteger(0)));

        let method_bytes = vec![
            0x14, 0x2C, 0x4D, 0x54, 0x44, 0x33, 0x01, 0x70, 0x68, 0x60, 0x7D, 0x79, 0x60, 0x0A,
            0x03, 0x00, 0x0A, 0xFF, 0x61, 0x7B, 0x7A, 0x60, 0x0A, 0x03, 0x00, 0x0A, 0xFF, 0x62,
            0xA0, 0x07, 0x90, 0x61, 0x62, 0xA4, 0x0A, 0x02, 0xA0, 0x06, 0x91, 0x61, 0x62, 0xA4,
            0x01, 0xA4, 0x00,
        ];
        assert_eq!(method.aml_bytes(), method_bytes);
    }
}
