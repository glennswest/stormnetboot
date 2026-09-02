//! NVMe/TCP initiator for firmware.
//!
//! Ported from sbregistry's `src/nvme.rs`, which is validated against real
//! hardware, rather than rebuilt from the specification — the wire format is
//! the part most likely to be subtly wrong, and the host implementation has
//! already paid for those lessons. Two of them are load-bearing and both are
//! preserved here with their reasoning:
//!
//!   * **PSDT = 01b on every command.** There are no PRPs over a fabric.
//!     Leaving the FLAGS byte zero says "PRPs are used", and a controller that
//!     validates it rejects the command with Invalid Field before it looks at
//!     the SGL. stormblockmk does not check; the Linux target does.
//!   * **The controller must be enabled before admin commands.** Fabrics
//!     Connect only establishes a queue. Identify before CC.EN is answered
//!     with Command Sequence Error on a conforming target.
//!
//! Differences from the host version, both forced by where this runs: it is
//! synchronous (firmware has no executor and the caller is a block driver that
//! wants blocking reads), and there is no write pipelining — a boot path reads
//! far more than it writes, and one command in flight keeps this small.

use alloc::format;
use alloc::string::{String, ToString};
use alloc::vec;
use alloc::vec::Vec;

use crate::tcp4::Tcp4Socket;

// PDU types.
const PDU_ICREQ: u8 = 0x00;
const PDU_ICRESP: u8 = 0x01;
const PDU_H2C_TERM: u8 = 0x02;
const PDU_C2H_TERM: u8 = 0x03;
const PDU_CMD: u8 = 0x04;
const PDU_RESP: u8 = 0x05;
const PDU_H2C_DATA: u8 = 0x06;
const PDU_C2H_DATA: u8 = 0x07;
const PDU_R2T: u8 = 0x09;

// NVMe opcodes.
const OPC_WRITE: u8 = 0x01;
const OPC_READ: u8 = 0x02;
const OPC_IDENTIFY: u8 = 0x06;
const OPC_FABRICS: u8 = 0x7F;
const FCTYPE_CONNECT: u8 = 0x01;
const FCTYPE_PROPERTY_SET: u8 = 0x00;
const FCTYPE_PROPERTY_GET: u8 = 0x04;

// Controller registers, reached over a fabric as properties.
const REG_CAP: u32 = 0x00;
const REG_CC: u32 = 0x14;
const REG_CSTS: u32 = 0x1C;

/// CC with EN=1, NVM command set, 4 KiB pages, round-robin, and the queue
/// entry sizes every controller uses: 2^6 submission, 2^4 completion.
const CC_ENABLE: u64 = (6 << 16) | (4 << 20) | 1;

/// SGL descriptor byte for data the transport moves (R2T/H2CData, C2HData).
const SGL_TRANSPORT: u8 = 0x5A;
/// SGL descriptor byte for data carried inside the capsule itself.
const SGL_IN_CAPSULE: u8 = 0x01;
/// FLAGS byte with PSDT = 01b — SGLs, the only choice over a fabric.
const FLAGS_SGL: u8 = 0x40;

/// Bytes moved per NVMe command, and so the size of the C2HData PDUs the
/// controller sends back.
///
/// **8 KiB: the portal is now on a jumbo network.** forge.g16.lo answers on
/// eth1 at MTU 9000, so one read command is exactly one frame — 8192 of
/// payload plus a 24-byte PDU header, 20 TCP, 20 IP and 14 Ethernet is 8270,
/// inside 9000 with room for options. No fragmentation, no reassembly.
///
/// Set it back to `64 * 1024` if the portal ever returns to a 1500-byte path:
/// this client has no read pipelining, so where nothing aligns to a frame
/// anyway, fewer larger commands beat more smaller ones.
///
/// Note that jumbo also needs the firmware to agree: EFI_TCP4's MTU comes
/// from the platform's IP4 configuration for that NIC, and `Tcp4Option`
/// carries `enable_path_mtu_discovery`, which this client does not set today.
const CHUNK: usize = 8192;

fn le16(b: &[u8], at: usize) -> u16 {
    u16::from_le_bytes([b[at], b[at + 1]])
}
fn le32(b: &[u8], at: usize) -> u32 {
    u32::from_le_bytes([b[at], b[at + 1], b[at + 2], b[at + 3]])
}
fn le64(b: &[u8], at: usize) -> u64 {
    let mut v = [0u8; 8];
    v.copy_from_slice(&b[at..at + 8]);
    u64::from_le_bytes(v)
}

/// One NVMe submission queue entry, built field by field: the layout is what
/// the wire cares about.
#[derive(Clone)]
struct Sqe([u8; 64]);

impl Sqe {
    fn new(opcode: u8, cid: u16, nsid: u32) -> Self {
        let mut s = [0u8; 64];
        s[0] = opcode;
        s[1] = FLAGS_SGL; // PSDT=01b — required on every fabrics command
        s[2..4].copy_from_slice(&cid.to_le_bytes());
        s[4..8].copy_from_slice(&nsid.to_le_bytes());
        Sqe(s)
    }
    /// Bytes 24..40 are the data pointer; as an SGL that is
    /// address(8) | length(4) | reserved(3) | type(1).
    fn sgl(&mut self, addr: u64, len: u32, kind: u8) -> &mut Self {
        self.0[24..32].copy_from_slice(&addr.to_le_bytes());
        self.0[32..36].copy_from_slice(&len.to_le_bytes());
        self.0[36..39].fill(0);
        self.0[39] = kind;
        self
    }
    fn dw(&mut self, n: usize, v: u32) -> &mut Self {
        let at = 40 + (n - 10) * 4;
        self.0[at..at + 4].copy_from_slice(&v.to_le_bytes());
        self
    }
    fn byte(&mut self, at: usize, v: u8) -> &mut Self {
        self.0[at] = v;
        self
    }
}

#[derive(Debug, Clone, Copy)]
struct Cqe {
    dw0: u32,
    dw1: u32,
    status: u16,
}

impl Cqe {
    fn parse(b: &[u8]) -> Self {
        Cqe {
            dw0: le32(b, 0),
            dw1: le32(b, 4),
            status: le16(b, 14),
        }
    }
    /// Bit 0 is the phase tag; status code and type sit above it.
    fn failed(&self) -> Option<String> {
        let sc = (self.status >> 1) & 0xFF;
        let sct = (self.status >> 9) & 0x7;
        (sc != 0 || sct != 0).then(|| format!("NVMe status sct={sct:#x} sc={sc:#04x}"))
    }
}

/// One NVMe/TCP queue: a TCP connection that has completed ICReq and Connect.
pub struct Queue {
    sock: Tcp4Socket,
    cid: u16,
    maxh2cdata: u32,
}

impl Queue {
    fn open(addr: [u8; 4], port: u16) -> Result<Self, String> {
        let sock = Tcp4Socket::connect(addr, port)?;
        let mut q = Queue {
            sock,
            cid: 0,
            maxh2cdata: 8192,
        };
        q.ic_handshake()?;
        Ok(q)
    }

    /// Negotiate the connection. Digests are declined, which keeps every later
    /// PDU free of trailing digest fields.
    fn ic_handshake(&mut self) -> Result<(), String> {
        let mut pdu = vec![0u8; 128];
        pdu[0] = PDU_ICREQ;
        pdu[2] = 128; // hlen
        pdu[4..8].copy_from_slice(&128u32.to_le_bytes()); // plen
        pdu[8..10].copy_from_slice(&0u16.to_le_bytes()); // pfv
        pdu[10] = 0; // hpda
        pdu[11] = 0; // no header or data digest
        pdu[12..16].copy_from_slice(&0u32.to_le_bytes()); // maxr2t = 1
        self.sock.send(&pdu)?;

        let resp = self.sock.read_exact(128)?;
        if resp[0] != PDU_ICRESP {
            return Err(format!(
                "expected ICResp, got PDU type {:#04x} (is this an NVMe/TCP port?)",
                resp[0]
            ));
        }
        if resp[11] & 0x3 != 0 {
            return Err("controller insists on PDU digests, which this client declined".into());
        }
        let maxh2cdata = le32(&resp, 12);
        if maxh2cdata >= 4096 {
            self.maxh2cdata = maxh2cdata;
        }
        Ok(())
    }

    fn next_cid(&mut self) -> u16 {
        self.cid = self.cid.wrapping_add(1);
        self.cid
    }

    fn send_cmd(&mut self, sqe: &Sqe, in_capsule: Option<&[u8]>) -> Result<(), String> {
        let data = in_capsule.unwrap_or(&[]);
        let hlen = 72u8; // 8 common header + 64 SQE
        let plen = hlen as u32 + data.len() as u32;
        let mut pdu = Vec::with_capacity(plen as usize);
        pdu.push(PDU_CMD);
        pdu.push(0); // flags
        pdu.push(hlen);
        pdu.push(if data.is_empty() { 0 } else { hlen }); // pdo
        pdu.extend_from_slice(&plen.to_le_bytes());
        pdu.extend_from_slice(&sqe.0);
        pdu.extend_from_slice(data);
        self.sock.send(&pdu)
    }

    fn send_h2c_data(
        &mut self,
        cid: u16,
        ttag: u16,
        offset: u32,
        data: &[u8],
        last: bool,
    ) -> Result<(), String> {
        let hlen: u8 = 24;
        let plen = hlen as u32 + data.len() as u32;
        let mut pdu = Vec::with_capacity(plen as usize);
        pdu.push(PDU_H2C_DATA);
        // Bit 2 is LAST_PDU in a data PDU; bits 0 and 1 are the digest flags.
        pdu.push(if last { 0x4 } else { 0 });
        pdu.push(hlen);
        pdu.push(hlen); // pdo
        pdu.extend_from_slice(&plen.to_le_bytes());
        pdu.extend_from_slice(&cid.to_le_bytes());
        pdu.extend_from_slice(&ttag.to_le_bytes());
        pdu.extend_from_slice(&offset.to_le_bytes());
        pdu.extend_from_slice(&(data.len() as u32).to_le_bytes());
        pdu.extend_from_slice(&0u32.to_le_bytes()); // reserved
        pdu.extend_from_slice(data);
        self.sock.send(&pdu)
    }

    /// Wait for `cid`, servicing whatever PDUs arrive on the way.
    fn complete(
        &mut self,
        cid: u16,
        write_data: Option<&[u8]>,
        mut read_into: Option<&mut [u8]>,
    ) -> Result<Cqe, String> {
        // Bounded so a controller that goes silent cannot wedge the boot.
        for _ in 0..4096 {
            let ch = self.sock.read_exact(8)?;
            let ptype = ch[0];
            let flags = ch[1];
            let hlen = ch[2] as usize;
            let pdo = ch[3] as usize;
            let plen = le32(&ch, 4) as usize;
            if plen < 8 || hlen < 8 {
                return Err(format!(
                    "malformed PDU (type {ptype:#04x}, hlen {hlen}, plen {plen})"
                ));
            }
            let rest = self.sock.read_exact(plen - 8)?;

            match ptype {
                PDU_RESP => {
                    let cqe = Cqe::parse(&rest[..16]);
                    if le16(&rest, 12) != cid {
                        continue; // not ours; nothing else is in flight here
                    }
                    return match cqe.failed() {
                        Some(msg) => Err(msg),
                        None => Ok(cqe),
                    };
                }
                PDU_R2T => {
                    let cccid = le16(&rest, 0);
                    let ttag = le16(&rest, 2);
                    let offset = le32(&rest, 4) as usize;
                    let length = le32(&rest, 8) as usize;
                    let data = write_data
                        .ok_or("controller asked for data on a command carrying none")?;
                    if offset + length > data.len() {
                        return Err(format!(
                            "controller asked for bytes {offset}..{} of a {}-byte transfer",
                            offset + length,
                            data.len()
                        ));
                    }
                    let mut sent = 0usize;
                    while sent < length {
                        let chunk = (length - sent).min(self.maxh2cdata as usize);
                        let last = sent + chunk == length;
                        self.send_h2c_data(
                            cccid,
                            ttag,
                            (offset + sent) as u32,
                            &data[offset + sent..offset + sent + chunk],
                            last,
                        )?;
                        sent += chunk;
                    }
                }
                PDU_C2H_DATA => {
                    let offset = le32(&rest, 4) as usize;
                    let length = le32(&rest, 8) as usize;
                    let payload_at = pdo.saturating_sub(8);
                    if payload_at + length > rest.len() {
                        return Err("C2HData payload runs past the PDU".into());
                    }
                    let payload = &rest[payload_at..payload_at + length];
                    let buf = read_into
                        .as_deref_mut()
                        .ok_or("unexpected read data on a command that asked for none")?;
                    if offset + length > buf.len() {
                        return Err("controller returned more data than requested".into());
                    }
                    buf[offset..offset + length].copy_from_slice(payload);
                    // In a data PDU bits 0 and 1 are digest flags, so LAST_PDU
                    // and SUCCESS are bits 2 and 3.
                    const C2H_LAST_PDU: u8 = 0x4;
                    const C2H_SUCCESS: u8 = 0x8;
                    if flags & C2H_LAST_PDU != 0 && flags & C2H_SUCCESS != 0 {
                        return Ok(Cqe {
                            dw0: 0,
                            dw1: 0,
                            status: 0,
                        });
                    }
                }
                PDU_C2H_TERM | PDU_H2C_TERM => {
                    return Err(format!(
                        "controller terminated the connection (PDU {ptype:#04x})"
                    ));
                }
                other => return Err(format!("unexpected PDU type {other:#04x}")),
            }
        }
        Err("controller never completed the command".to_string())
    }

    fn property_get(&mut self, offset: u32, eight: bool) -> Result<u64, String> {
        let cid = self.next_cid();
        let mut sqe = Sqe::new(OPC_FABRICS, cid, 0);
        sqe.byte(4, FCTYPE_PROPERTY_GET)
            .byte(40, if eight { 1 } else { 0 })
            .dw(11, offset);
        self.send_cmd(&sqe, None)?;
        let cqe = self.complete(cid, None, None)?;
        Ok(if eight {
            (cqe.dw0 as u64) | ((cqe.dw1 as u64) << 32)
        } else {
            cqe.dw0 as u64
        })
    }

    fn property_set(&mut self, offset: u32, value: u64, eight: bool) -> Result<(), String> {
        let cid = self.next_cid();
        let mut sqe = Sqe::new(OPC_FABRICS, cid, 0);
        sqe.byte(4, FCTYPE_PROPERTY_SET)
            .byte(40, if eight { 1 } else { 0 })
            .dw(11, offset)
            .dw(12, value as u32)
            .dw(13, (value >> 32) as u32);
        self.send_cmd(&sqe, None)?;
        self.complete(cid, None, None).map(|_| ())
    }

    /// Fabrics Connect. `cntlid` is 0xFFFF on the admin queue (dynamic), and
    /// the id the admin Connect returned on an I/O queue.
    fn fabrics_connect(
        &mut self,
        qid: u16,
        sqsize: u16,
        cntlid: u16,
        subnqn: &str,
        hostnqn: &str,
        hostid: &[u8; 16],
    ) -> Result<u16, String> {
        let mut data = vec![0u8; 1024];
        data[0..16].copy_from_slice(hostid);
        data[16..18].copy_from_slice(&cntlid.to_le_bytes());
        let put = |buf: &mut [u8], s: &str| {
            let b = s.as_bytes();
            let n = b.len().min(255);
            buf[..n].copy_from_slice(&b[..n]);
        };
        put(&mut data[256..512], subnqn);
        put(&mut data[512..768], hostnqn);

        let cid = self.next_cid();
        let mut sqe = Sqe::new(OPC_FABRICS, cid, 0);
        sqe.byte(4, FCTYPE_CONNECT)
            .sgl(0, data.len() as u32, SGL_IN_CAPSULE)
            // CDW10: RECFMT(0) low half, QID high half.
            .dw(10, (qid as u32) << 16)
            // CDW11: SQSIZE, 0-based.
            .dw(11, sqsize.saturating_sub(1) as u32);
        self.send_cmd(&sqe, Some(&data))?;
        let cqe = self.complete(cid, None, None)?;
        Ok((cqe.dw0 & 0xFFFF) as u16)
    }

    /// Bring the controller out of reset.
    ///
    /// Connect only establishes a queue; a conforming target answers every
    /// admin command before this with Command Sequence Error.
    fn enable_controller(&mut self) -> Result<u16, String> {
        let cap = self.property_get(REG_CAP, true)?;
        let mqes = ((cap & 0xFFFF) as u16).saturating_add(1);
        // CAP.TO is in 500ms units.
        let timeout_ms = ((cap >> 24) & 0xFF).max(1) * 500;

        self.property_set(REG_CC, CC_ENABLE, false)?;

        let mut waited = 0u64;
        loop {
            let csts = self.property_get(REG_CSTS, false)?;
            if csts & 0x2 != 0 {
                return Err("controller reports fatal status (CSTS.CFS)".into());
            }
            if csts & 0x1 != 0 {
                return Ok(mqes);
            }
            if waited > timeout_ms {
                return Err(format!("controller not ready {timeout_ms}ms after CC.EN"));
            }
            uefi::boot::stall(core::time::Duration::from_millis(10));
            waited += 10;
        }
    }
}

/// Geometry of the namespace being booted from.
#[derive(Debug, Clone, Copy)]
pub struct Geometry {
    pub blocks: u64,
    pub block_size: u32,
}

/// An attached namespace: admin queue enabled, I/O queue connected.
pub struct Namespace {
    io: Queue,
    pub nsid: u32,
    pub geometry: Geometry,
    /// Largest transfer the controller accepts, in bytes.
    pub max_transfer: usize,
}

impl Namespace {
    /// Connect, enable, identify, and open an I/O queue.
    pub fn attach(
        addr: [u8; 4],
        port: u16,
        subnqn: &str,
        nsid: u32,
        hostnqn: &str,
    ) -> Result<Self, String> {
        // A stable host id derived from the host NQN: the target uses it to
        // recognise this initiator across reconnects, and firmware has no
        // process id or randomness to fall back on.
        let mut hostid = [0u8; 16];
        for (i, b) in hostnqn.as_bytes().iter().take(16).enumerate() {
            hostid[i] = *b;
        }

        let mut admin = Queue::open(addr, port)?;
        let cntlid = admin.fabrics_connect(0, 32, 0xFFFF, subnqn, hostnqn, &hostid)?;
        let mqes = admin.enable_controller()?;

        // Identify the namespace: CNS 0x00, nsid in the command.
        let cid = admin.next_cid();
        let mut sqe = Sqe::new(OPC_IDENTIFY, cid, nsid);
        sqe.sgl(0, 4096, SGL_TRANSPORT).dw(10, 0x00);
        let mut idns = vec![0u8; 4096];
        admin.send_cmd(&sqe, None)?;
        admin.complete(cid, None, Some(&mut idns))?;

        // NSZE at 0, FLBAS at 26, LBA format table at 128 (16 entries of 4
        // bytes; byte 2 of an entry is the LBA data size as a power of two).
        let blocks = le64(&idns, 0);
        let flbas = (idns[26] & 0x0F) as usize;
        let lbads = idns[128 + flbas * 4 + 2];
        let block_size = 1u32 << lbads;
        if block_size < 512 || block_size > 65536 {
            return Err(format!("implausible block size {block_size} (LBADS {lbads})"));
        }

        // A second connection for I/O, carrying the controller id the admin
        // Connect handed back.
        let mut io = Queue::open(addr, port)?;
        let sqsize = mqes.min(128);
        io.fabrics_connect(1, sqsize, cntlid, subnqn, hostnqn, &hostid)?;

        Ok(Namespace {
            io,
            nsid,
            geometry: Geometry { blocks, block_size },
            // See CHUNK: 64 KiB on a 1500 path, 8 KiB once it is jumbo.
            max_transfer: CHUNK,
        })
    }

    pub fn read(&mut self, lba: u64, buf: &mut [u8]) -> Result<(), String> {
        let bs = self.geometry.block_size as usize;
        if buf.len() % bs != 0 {
            return Err(format!("read of {} bytes is not a multiple of {bs}", buf.len()));
        }
        let mut done = 0usize;
        while done < buf.len() {
            let chunk = (buf.len() - done).min(self.max_transfer);
            let blocks = (chunk / bs) as u32;
            let cid = self.io.next_cid();
            let at = lba + (done / bs) as u64;
            let mut sqe = Sqe::new(OPC_READ, cid, self.nsid);
            sqe.sgl(0, chunk as u32, SGL_TRANSPORT)
                .dw(10, at as u32)
                .dw(11, (at >> 32) as u32)
                // CDW12 NLB is 0-based.
                .dw(12, blocks - 1);
            self.io.send_cmd(&sqe, None)?;
            self.io
                .complete(cid, None, Some(&mut buf[done..done + chunk]))?;
            done += chunk;
        }
        Ok(())
    }

    pub fn write(&mut self, lba: u64, data: &[u8]) -> Result<(), String> {
        let bs = self.geometry.block_size as usize;
        if data.len() % bs != 0 {
            return Err(format!(
                "write of {} bytes is not a multiple of {bs}",
                data.len()
            ));
        }
        let mut done = 0usize;
        while done < data.len() {
            let chunk = (data.len() - done).min(self.max_transfer);
            let blocks = (chunk / bs) as u32;
            let cid = self.io.next_cid();
            let at = lba + (done / bs) as u64;
            let mut sqe = Sqe::new(OPC_WRITE, cid, self.nsid);
            sqe.sgl(0, chunk as u32, SGL_TRANSPORT)
                .dw(10, at as u32)
                .dw(11, (at >> 32) as u32)
                .dw(12, blocks - 1);
            self.io.send_cmd(&sqe, None)?;
            self.io
                .complete(cid, Some(&data[done..done + chunk]), None)?;
            done += chunk;
        }
        Ok(())
    }
}
