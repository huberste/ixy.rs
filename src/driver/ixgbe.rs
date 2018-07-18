use driver::*;
use std::thread;
use std::time::Duration;
use std::ptr;
use std::error::Error;

use self::constants::*;
use self::pci::*;

use std::rc::Rc;
use std::cell::RefCell;

use std::collections::VecDeque;

const DRIVER_NAME: &str = "ixy-ixgbe";

const MAX_RX_QUEUE_ENTRIES: u32 = 4096;
const MAX_TX_QUEUE_ENTRIES: u32 = 4096;

const NUM_RX_QUEUE_ENTRIES: u32 = 512;
const NUM_TX_QUEUE_ENTRIES: u32 = 512;

const TX_CLEAN_BATCH: u32 = 32;

const fn wrap_ring(index: u32, ring_size: u32) -> u32 {
    (index + 1) & (ring_size - 1)
}

pub struct IxgbeDevice {
    addr: *mut u8,
    len: usize,
    num_rx_queues: u32,
    num_tx_queues: u32,
    rx_queues: Vec<IxgbeRxQueue>,
    tx_queues: Vec<IxgbeTxQueue>,
}

struct IxgbeRxQueue {
    descriptors: *mut ixgbe_adv_rx_desc,
    mempool: Rc<RefCell<Mempool>>,
    num_entries: u32,
    rx_index: u32,
    mempool_entries: Vec<u32>,
}

struct IxgbeTxQueue {
    descriptors: *mut ixgbe_adv_tx_desc,
    queue: VecDeque<Packet>,
    num_entries: u32,
    clean_index: u32,
    tx_index: u32,
}

fn reset_and_init(ixgbe: &mut IxgbeDevice) {
    let mut huge_page_id: u32 = 0;

    // section 4.6.3.1 - disable all interrupts
    ixgbe.set_reg32(IXGBE_EIMC, 0x7FFFFFFF);

    // section 4.6.3.2
    ixgbe.set_reg32(IXGBE_CTRL, IXGBE_CTRL_RST_MASK);
    ixgbe.wait_clear_reg32(IXGBE_CTRL, IXGBE_CTRL_RST_MASK);
    thread::sleep(Duration::from_millis(10));

    // section 4.6.3.1 - disable interrupts again after reset
    ixgbe.set_reg32(IXGBE_EIMC, 0x7FFFFFFF);

    println!("initializing device");

    // section 4.6.3 - wait for EEPROM auto read completion
    ixgbe.wait_set_reg32(IXGBE_EEC, IXGBE_EEC_ARD);

    // section 4.6.3 - wait for dma initialization done
    ixgbe.wait_set_reg32(IXGBE_RDRXCTL, IXGBE_RDRXCTL_DMAIDONE);

    println!("initializing link");

    // section 4.6.4 - initialize link (auto negotiation)
    init_link(ixgbe);

    println!("resetting stats");

    // section 4.6.5 - reset registers
    ixgbe.reset_stats();

    println!("initializing rx");

    // section 4.6.7 - init rx
    init_rx(ixgbe, &mut huge_page_id);

    println!("initializing tx");

    // section 4.6.8 - init tx
    init_tx(ixgbe, &mut huge_page_id);

    println!("starting rx queues");

    for i in 0..ixgbe.num_rx_queues {
        start_rx_queue(ixgbe, i, &mut huge_page_id);
    }

    println!("starting tx queues");

    for i in 0..ixgbe.num_tx_queues {
        start_tx_queue(ixgbe, i);
    }

    println!("starting promisc mode");

    ixgbe.set_promisc(true);

    println!("waiting for link");

    wait_for_link(ixgbe);
}

// sections 4.6.7
fn init_rx(ixgbe: &mut IxgbeDevice, huge_page_id: &mut u32) {
    // disable rx while re-configuring
    ixgbe.clear_flags32(IXGBE_RXCTRL, IXGBE_RXCTRL_RXEN);

    // section 4.6.11.3.4 - allocate all queues and traffic to PB0
    ixgbe.set_reg32(IXGBE_RXPBSIZE(0), IXGBE_RXPBSIZE_128KB);
    for i in 1..8 {
        ixgbe.set_reg32(IXGBE_RXPBSIZE(i), 0);
    }

    // enable CRC offloading
    ixgbe.set_flags32(IXGBE_HLREG0, IXGBE_HLREG0_RXCRCSTRP);
    ixgbe.set_flags32(IXGBE_RDRXCTL, IXGBE_RDRXCTL_CRCSTRIP);

    // accept broadcast packets
    ixgbe.set_flags32(IXGBE_FCTRL, IXGBE_FCTRL_BAM);

    // configure queues
    for i in 0..ixgbe.num_rx_queues {
        ixgbe.set_reg32(IXGBE_SRRCTL(i), (ixgbe.get_reg32(IXGBE_SRRCTL(i)) & !IXGBE_SRRCTL_DESCTYPE_MASK) | IXGBE_SRRCTL_DESCTYPE_ADV_ONEBUF);

        ixgbe.set_flags32(IXGBE_SRRCTL(i), IXGBE_SRRCTL_DROP_EN);

        // section 7.1.9 - setup descriptor ring
        let ring_size_bytes = (NUM_RX_QUEUE_ENTRIES) * mem::size_of::<ixgbe_adv_rx_desc>() as u32;

        // TODO check result of allocate_dma_memory
        let dma = DmaMemory::allocate(huge_page_id, ring_size_bytes).unwrap();

        unsafe { memset(dma.virt, ring_size_bytes, 0xff); }

        ixgbe.set_reg32(IXGBE_RDBAL(i), (dma.phys as u64 & 0xffffffff) as u32);
        ixgbe.set_reg32(IXGBE_RDBAH(i), (dma.phys as u64 >> 32) as u32);
        ixgbe.set_reg32(IXGBE_RDLEN(i), ring_size_bytes as u32);

        ixgbe.set_reg32(IXGBE_RDH(i), 0);
        ixgbe.set_reg32(IXGBE_RDT(i), 0);

        let mempool_size = if NUM_RX_QUEUE_ENTRIES + NUM_TX_QUEUE_ENTRIES < 4096 {
            4096
        } else {
            NUM_RX_QUEUE_ENTRIES + NUM_TX_QUEUE_ENTRIES
        };

        let mempool = Rc::new(
            RefCell::new(
                Mempool::allocate(huge_page_id, mempool_size, 2048).unwrap()
            )
        );

        let rx_queue = IxgbeRxQueue {
            descriptors: dma.virt as *mut ixgbe_adv_rx_desc,
            mempool,
            num_entries: NUM_RX_QUEUE_ENTRIES,
            rx_index: 0,
            mempool_entries: Vec::new(),
        };

        ixgbe.rx_queues.push(rx_queue);
    }

    // last sentence of section 4.6.7
    ixgbe.set_flags32(IXGBE_CTRL_EXT, IXGBE_CTRL_EXT_NS_DIS);

    for i in 0..ixgbe.num_rx_queues {
        ixgbe.clear_flags32(IXGBE_DCA_RXCTRL(i), 1 << 12);
    }

    // start rx
    ixgbe.set_flags32(IXGBE_RXCTRL, IXGBE_RXCTRL_RXEN);
}

// section 4.6.8
fn init_tx(ixgbe: &mut IxgbeDevice, huge_page_id: &mut u32) {
    // crc offload
    ixgbe.set_flags32(IXGBE_HLREG0, IXGBE_HLREG0_TXCRCEN | IXGBE_HLREG0_TXPADEN);

    // section 4.6.11.3.4
    ixgbe.set_reg32(IXGBE_TXPBSIZE(0), IXGBE_TXPBSIZE_40KB);
    for i in 1..8 {
        ixgbe.set_reg32(IXGBE_TXPBSIZE(i), 0);
    }

    // required when not using DCB/VTd
    ixgbe.set_reg32(IXGBE_DTXMXSZRQ, 0xffff);
    ixgbe.clear_flags32(IXGBE_RTTDCS, IXGBE_RTTDCS_ARBDIS);

    // configure queues
    for i in 0..ixgbe.num_tx_queues {
        // setup descriptor ring, see section 7.1.9
        let ring_size_bytes = NUM_TX_QUEUE_ENTRIES * mem::size_of::<ixgbe_adv_tx_desc>() as u32;

        // TODO check result of allocate_dma_memory
        let dma = DmaMemory::allocate(huge_page_id, ring_size_bytes).unwrap();
        unsafe { memset(dma.virt, ring_size_bytes, 0xff); }

        ixgbe.set_reg32(IXGBE_TDBAL(i), (dma.phys as u64 & 0xffffffff) as u32);
        ixgbe.set_reg32(IXGBE_TDBAH(i), (dma.phys as u64 >> 32) as u32);
        ixgbe.set_reg32(IXGBE_TDLEN(i), ring_size_bytes as u32);

        let mut txdctl = ixgbe.get_reg32(IXGBE_TXDCTL(i));

        txdctl &= !(0x3F | (0x3F << 8) | (0x3F << 16));
        txdctl |= 36 | (8 << 8) | (4 << 16);

        ixgbe.set_reg32(IXGBE_TXDCTL(i), txdctl);

        let tx_queue = IxgbeTxQueue {
            descriptors: dma.virt as *mut ixgbe_adv_tx_desc,
            queue: VecDeque::new(),
            num_entries: NUM_RX_QUEUE_ENTRIES,
            clean_index: 0,
            tx_index: 0,
        };

        ixgbe.tx_queues.push(tx_queue);
    }

    // final step: enable DMA
    ixgbe.set_reg32(IXGBE_DMATXCTL, IXGBE_DMATXCTL_TE);
}

fn start_rx_queue(ixgbe: &mut IxgbeDevice, queue_id: u32, huge_page_id: &mut u32) {
    {
        let queue = &mut ixgbe.rx_queues[queue_id as usize];

        if queue.num_entries & (queue.num_entries - 1) != 0 {
            panic!("number of queue entries must be a power of 2");
        }

        for i in 0..queue.num_entries {
            let pool = &queue.mempool;
            let buf = pool.borrow_mut().pkt_buf_alloc();

            unsafe {
                // write to ixgbe_adv_rx_desc.read.pkt_addr
                ptr::write_volatile(queue.descriptors.offset(i as isize) as *mut u64, virt_to_phys(pool.borrow().offset(buf) as usize).unwrap() as u64);
                // write to ixgbe_adv_rx_desc.read.hdr_addr
                ptr::write_volatile((queue.descriptors.offset(i as isize) as usize + mem::size_of::<u64>()) as *mut u64, 0);
            }

            queue.mempool_entries.push(buf);
        }
    }

    let queue = &ixgbe.rx_queues[queue_id as usize];

    ixgbe.set_flags32(IXGBE_RXDCTL(queue_id), IXGBE_RXDCTL_ENABLE);
    ixgbe.wait_set_reg32(IXGBE_RXDCTL(queue_id), IXGBE_RXDCTL_ENABLE);

    // rx queue starts out full
    ixgbe.set_reg32(IXGBE_RDH(queue_id), 0);

    // was set to 0 before in the init function
    ixgbe.set_reg32(IXGBE_RDT(queue_id), queue.num_entries - 1);
}

fn start_tx_queue(ixgbe: &mut IxgbeDevice, queue_id: u32) {
    {
        let queue = &mut ixgbe.tx_queues[queue_id as usize];

        let mempool_size = NUM_RX_QUEUE_ENTRIES * NUM_TX_QUEUE_ENTRIES;

        if queue.num_entries & (queue.num_entries - 1) != 0 {
            println!("number of queue entries must be a power of 2");
        }
    }

    // tx queue starts out empty
    ixgbe.set_reg32(IXGBE_TDH(queue_id), 0);
    ixgbe.set_reg32(IXGBE_TDT(queue_id), 0);

    // enable queue and wait if necessary
    ixgbe.set_flags32(IXGBE_TXDCTL(queue_id), IXGBE_TXDCTL_ENABLE);
    ixgbe.wait_set_reg32(IXGBE_TXDCTL(queue_id), IXGBE_TXDCTL_ENABLE);
}

fn ixgbe_rx_batch(ixgbe: &mut IxgbeDevice, queue_id: u32, num_bufs: u32) -> Vec<Packet> {
    let mut packets: Vec<Packet> = Vec::new();

    let mut rx_index = 0;
    let mut last_rx_index = rx_index;

    {
        let queue = &mut ixgbe.rx_queues[queue_id as usize];

        rx_index = queue.rx_index;

        for i in 0..num_bufs {
            let status = unsafe { ptr::read_volatile((queue.descriptors.offset(rx_index as isize) as usize + 2 * mem::size_of::<u32>()) as *const u32) };

            if (status & IXGBE_RXDADV_STAT_DD) != 0 {
                if (status & IXGBE_RXDADV_STAT_EOP) == 0 {
                    panic!("increase buffer size or decrease MTU")
                }

                let mut pool = queue.mempool.borrow_mut();

                let addr = pool.offset(queue.mempool_entries[rx_index as usize]);
                let len = unsafe { ptr::read_volatile((queue.descriptors.offset(rx_index as isize) as usize + 3 * mem::size_of::<u32>()) as *mut u32) as usize };
                let mempool_entry = queue.mempool_entries[rx_index as usize];

                let p = Packet::new(addr, len, &queue.mempool, mempool_entry);

                packets.push(p);

                let buf = pool.pkt_buf_alloc();

                unsafe {
                    // write to ixgbe_adv_rx_desc.read.pkt_addr
                    ptr::write_volatile(queue.descriptors.offset(rx_index as isize) as *mut u64, virt_to_phys(pool.offset(buf) as usize).unwrap() as u64);
                    // write to ixgbe_adv_rx_desc.read.hdr_addr
                    ptr::write_volatile((queue.descriptors.offset(rx_index as isize) as usize + mem::size_of::<u64>()) as *mut u64, 0);
                }

                queue.mempool_entries[rx_index as usize] = buf;

                last_rx_index = rx_index;
                rx_index = wrap_ring(rx_index, queue.num_entries);
            } else {
                break;
            }
        }
    }

    if rx_index != last_rx_index {
        ixgbe.set_reg32(IXGBE_RDT(queue_id), last_rx_index);
        ixgbe.rx_queues[queue_id as usize].rx_index = rx_index;
    }

    thread::sleep(Duration::from_millis(100));

    packets
}

fn ixgbe_tx_batch(ixgbe: &mut IxgbeDevice, queue_id: u32, packets: Vec<Packet>) -> u32 {
    let mut sent = 0;

    {
        let queue = &mut ixgbe.tx_queues[queue_id as usize];

        let mut clean_index = queue.clean_index;
        let mut cur_index = queue.tx_index;

        loop {
            let mut cleanable = cur_index as i32 - clean_index as i32;

            if cleanable < 0 {
                cleanable = queue.num_entries as i32 + cleanable;
            }

            if (cleanable as u32) < TX_CLEAN_BATCH {
                break;
            }

            let mut cleanup_to = clean_index + TX_CLEAN_BATCH - 1;

            if cleanup_to >= queue.num_entries {
                cleanup_to = cleanup_to - queue.num_entries;
            }

            let status = unsafe { ptr::read_volatile((queue.descriptors.offset(cleanup_to as isize) as usize + mem::size_of::<u64>() + mem::size_of::<u32>()) as *mut u32) };

            if (status & IXGBE_ADVTXD_STAT_DD) != 0 {
                for _ in 0..cleanable {
                    queue.queue.pop_front();
                }
                clean_index = wrap_ring(cleanup_to, queue.num_entries);
            } else {
                break;
            }
        }

        queue.clean_index = clean_index;

        for packet in packets {
            let next_index = wrap_ring(cur_index, queue.num_entries);

            if clean_index == next_index {
                return sent as u32
            }

            queue.tx_index = wrap_ring(queue.tx_index, queue.num_entries);

            unsafe {
                // write to read.buffer_addr
                ptr::write_volatile(queue.descriptors.offset(cur_index as isize) as *mut u64, virt_to_phys(packet.get_addr() as usize).unwrap() as u64);
                // write to read.buffer_addr
                ptr::write_volatile((queue.descriptors.offset(cur_index as isize) as usize + mem::size_of::<u64>()) as *mut u32, IXGBE_ADVTXD_DCMD_EOP | IXGBE_ADVTXD_DCMD_RS | IXGBE_ADVTXD_DCMD_IFCS | IXGBE_ADVTXD_DCMD_DEXT | IXGBE_ADVTXD_DTYP_DATA | packet.len() as u32);
                // write to read.olinfo_status
                ptr::write_volatile((queue.descriptors.offset(cur_index as isize) as usize + mem::size_of::<u64>() + mem::size_of::<u32>()) as *mut u32, (packet.len() as u32) << IXGBE_ADVTXD_PAYLEN_SHIFT);
            }

            queue.queue.push_back(packet);

            cur_index = next_index;
            sent = sent + 1;
        }
    }

    ixgbe.set_reg32(IXGBE_TDT(queue_id), ixgbe.tx_queues[queue_id as usize].tx_index);

    sent
}

impl IxyDriver for IxgbeDevice {
    fn init(pci_addr: &str, num_rx_queues: u32, num_tx_queues: u32) -> Result<IxgbeDevice, Box<Error>> {
        if num_rx_queues > MAX_QUEUES || num_tx_queues > MAX_QUEUES {
            panic!("too many queues")
        }

        println!("pci mapping device");

        let (addr, len) = pci_map(pci_addr)?;

        let rx_queues = Vec::new();
        let tx_queues = Vec::new();
        let mut dev = IxgbeDevice { addr, len, num_rx_queues, num_tx_queues, rx_queues, tx_queues };

        reset_and_init(&mut dev);

        Ok(dev)
    }

    fn driver_name(&self) -> &str {
        DRIVER_NAME
    }

    fn rx_batch(&mut self, queue_id: u32, num_packets: u32) -> Vec<Packet> {
        ixgbe_rx_batch(self, queue_id, num_packets)
    }

    fn tx_batch(&mut self, queue_id: u32, packets: Vec<Packet>) -> u32 {
        ixgbe_tx_batch(self, queue_id, packets)
    }

    fn read_stats(&self, stats: &mut DeviceStats) {
        let rx_pkts = self.get_reg32(IXGBE_GPRC) as u64;
        let tx_pkts = self.get_reg32(IXGBE_GPTC) as u64;
        let rx_bytes = self.get_reg32(IXGBE_GORCL) as u64 + ((self.get_reg32(IXGBE_GORCH) as u64) << 32);
        let tx_bytes = self.get_reg32(IXGBE_GOTCL) as u64 + ((self.get_reg32(IXGBE_GOTCH) as u64) << 32);

        stats.rx_pkts += rx_pkts;
        stats.tx_pkts += tx_pkts;
        stats.rx_bytes += rx_bytes;
        stats.tx_bytes += tx_bytes;
    }

    fn reset_stats(&self) {
        let rx_pkts = self.get_reg32(IXGBE_GPRC) as u64;
        let tx_pkts = self.get_reg32(IXGBE_GPTC) as u64;
        let rx_bytes = self.get_reg32(IXGBE_GORCL) as u64 + ((self.get_reg32(IXGBE_GORCH) as u64) << 32);
        let tx_bytes = self.get_reg32(IXGBE_GOTCL) as u64 + ((self.get_reg32(IXGBE_GOTCH) as u64) << 32);
    }

    fn set_promisc(&self, enabled: bool) {
        if enabled {
            println!("enabling promisc mode");
            self.set_flags32(IXGBE_FCTRL, IXGBE_FCTRL_MPE | IXGBE_FCTRL_UPE);
        } else {
            println!("disabling promisc mode");
            self.clear_flags32(IXGBE_FCTRL, IXGBE_FCTRL_MPE | IXGBE_FCTRL_UPE);
        }
    }

    fn get_link_speed(&self) -> u16 {
        let speed = self.get_reg32(IXGBE_LINKS);
        if (speed & IXGBE_LINKS_UP) == 0 {
            return 0;
        }
        match speed & IXGBE_LINKS_SPEED_82599 {
            IXGBE_LINKS_SPEED_100_82599 => 100,
            IXGBE_LINKS_SPEED_1G_82599 => 1000,
            IXGBE_LINKS_SPEED_10G_82599 => 10000,
            _ => 0,
        }
    }
}

impl IxgbeDevice {
    fn get_reg32(&self, reg: u32) -> u32 {
        if reg as usize <= self.len - 4 as usize {
            unsafe { ptr::read_volatile((self.addr as usize + reg as usize) as *mut u32) }
        } else {
            panic!("memory access is out of bounds");
        }
    }

    fn set_reg32(&self, reg: u32, value: u32) {
        if reg as usize <= self.len - 4 as usize {
            unsafe { ptr::write_volatile((self.addr as usize + reg as usize) as *mut u32, value); }
        } else {
            panic!("memory access is out of bounds");
        }
    }

    fn set_flags32(&self, reg: u32, flags: u32) {
        self.set_reg32(reg, self.get_reg32(reg) | flags);
    }

    fn clear_flags32(&self, reg: u32, flags: u32) {
        self.set_reg32(reg, self.get_reg32(reg) & !flags);
    }

    fn wait_clear_reg32(&self, reg: u32, value: u32) {
        loop {
            let current = self.get_reg32(reg);
            if (current & value) == 0 {
                break;
            }
            println!("Register: {:x}, current: {:x}, value: {:x}, expected: {:x}", reg, current, value, 0);
            thread::sleep(Duration::from_millis(100));
        }
    }

    fn wait_set_reg32(&self, reg: u32, value: u32) {
        loop {
            let current = self.get_reg32(reg);
            if (current & value) == value {
                break;
            }
            println!("Register: {:x}, current: {:x}, value: {:x}, expected: ~{:x}", reg, current, value, value);
            thread::sleep(Duration::from_millis(100));
        }
    }
}

// see section 4.6.4
fn init_link(ixgbe: &IxgbeDevice) {
    ixgbe.set_reg32(IXGBE_AUTOC, (ixgbe.get_reg32(IXGBE_AUTOC) & !IXGBE_AUTOC_LMS_MASK) | IXGBE_AUTOC_LMS_10G_SERIAL);
    ixgbe.set_reg32(IXGBE_AUTOC, (ixgbe.get_reg32(IXGBE_AUTOC) & !IXGBE_AUTOC_10G_PMA_PMD_MASK) | IXGBE_AUTOC_10G_XAUI);
    // negotiate link
    ixgbe.set_flags32(IXGBE_AUTOC, IXGBE_AUTOC_AN_RESTART);
}

fn wait_for_link(ixgbe: &IxgbeDevice) {
    let mut max_wait = 10000; // 10 seconds
    let poll_interval = 10;
    let speed = ixgbe.get_link_speed();
    while speed == 0 && max_wait > 0 {
        thread::sleep(Duration::from_millis(poll_interval));
        max_wait -= poll_interval;
    }
    println!("Link speed is {} Mbit/s", ixgbe.get_link_speed());
}

unsafe fn get_reg32(addr: usize, reg: u32) -> u32 {
    ptr::read_volatile((addr + reg as usize) as *mut u32)
}

unsafe fn set_reg32(addr: usize, reg: u32, value: u32) {
    ptr::write_volatile((addr + reg as usize) as *mut u32, value);
}

unsafe fn set_flags32(addr: usize, reg: u32, flags: u32) {
    set_reg32(addr, reg, get_reg32(addr, reg) | flags);
}

unsafe fn clear_flags32(addr: usize, reg: u32, flags: u32) {
    set_reg32(addr, reg, get_reg32(addr, reg) & !flags);
}

unsafe fn wait_clear_reg32(data: usize, register: u32, value: u32) {
    loop {
        let current = ptr::read_volatile((data + register as usize) as *const u32);
        if (current & value) == 0 {
            break;
        }
        println!("Register: {:x}, current: {:x}, value: {:x}, expected: {:x}", register, current, value, 0);
        thread::sleep(Duration::from_millis(100));
    }
}

unsafe fn wait_set_reg32(data: usize, register: u32, value: u32) {
    loop {
        let current = ptr::read_volatile((data + register as usize) as *const u32);
        if (current & value) == value {
            break;
        }
        println!("Register: {:x}, current: {:x}, value: {:x}, expected: ~{:x}", register, current, value, value);
        thread::sleep(Duration::from_millis(100));
    }
}
