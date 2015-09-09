use core::ops::DerefMut;

use common::memory::*;
use common::pci::*;
use common::pio::*;
use common::scheduler::*;

use network::common::*;
use network::ethernet::*;

use programs::common::*;

pub struct RTL8139Resource {
    pub nic: *mut RTL8139,
    pub ptr: *mut RTL8139Resource,
    pub inbound: Queue<Vec<u8>>,
    pub outbound: Queue<Vec<u8>>
}

impl RTL8139Resource {
    pub fn new(nic: &mut RTL8139) -> Box<RTL8139Resource> {
        let mut ret = box RTL8139Resource {
            nic: nic,
            ptr: 0 as *mut RTL8139Resource,
            inbound: Queue::new(),
            outbound: Queue::new()
        };

        unsafe{
            ret.ptr = ret.deref_mut();

            if ret.nic as usize > 0 && ret.ptr as usize > 0 {
                let reenable = start_no_ints();

                (*ret.nic).resources.push(ret.ptr);

                end_no_ints(reenable);
            }
        }

        return ret;
    }
}

impl Resource for RTL8139Resource {
    fn url(&self) -> URL {
        return URL::from_string(&"network://".to_string());
    }

    fn stat(&self) -> ResourceType {
        return ResourceType::File;
    }

    fn read(&mut self, buf: &mut [u8]) -> Option<usize> {
        loop {
            let option;
            unsafe{
                let reenable = start_no_ints();
                option = self.inbound.pop();
                end_no_ints(reenable);
            }

            if let Option::Some(bytes) = option {
                let mut i = 0;
                while i < buf.len() {
                    match bytes.get(i) {
                        Option::Some(byte) => buf[i] = *byte,
                        Option::None => break
                    }
                    i += 1;
                }
                return Option::Some(i);
            }

            sys_yield();
        }
    }

    fn read_to_end(&mut self, vec: &mut Vec<u8>) -> Option<usize> {
        dh(self as *mut RTL8139Resource as usize);
        dl();

        loop {
            let option;
            unsafe{
                let reenable = start_no_ints();
                option = self.inbound.pop();
                end_no_ints(reenable);
            }

            if let Option::Some(bytes) = option {
                vec.push_all(&bytes);
                return Option::Some(bytes.len());
            }

            sys_yield();
        }
    }

    fn write(&mut self, buf: &[u8]) -> Option<usize> {
        unsafe{
            let reenable = start_no_ints();
            self.outbound.push(Vec::from_raw_buf(buf.as_ptr(), buf.len()));
            end_no_ints(reenable);

            if self.nic as usize > 0 {
                (*self.nic).send_outbound();
            }
        }

        return Option::Some(buf.len());
    }

    fn seek(&mut self, pos: ResourceSeek) -> Option<usize> {
        return Option::None;
    }

    fn flush(&mut self) -> bool {
        loop {
            let len;
            unsafe{
                let reenable = start_no_ints();
                len = self.outbound.len();
                end_no_ints(reenable);
            }

            if len == 0 {
                return true;
            }

            sys_yield();
        }
    }
}

impl Drop for RTL8139Resource {
    fn drop(&mut self){
        if self.nic as usize > 0 && self.ptr as usize > 0 {
            unsafe {
                let reenable = start_no_ints();

                let mut i = 0;
                while i < (*self.nic).resources.len() {
                    let mut remove = false;

                    match (*self.nic).resources.get(i) {
                        Option::Some(ptr) => if *ptr == self.ptr {
                            remove = true;
                        }else{
                            i += 1;
                        },
                        Option::None => break
                    }

                    if remove {
                        (*self.nic).resources.remove(i);
                    }
                }

                end_no_ints(reenable);
            }
        }
    }
}

pub struct RTL8139 {
    pub bus: usize,
    pub slot: usize,
    pub func: usize,
    pub base: usize,
    pub memory_mapped: bool,
    pub irq: u8,
    pub resources: Vec<*mut RTL8139Resource>,
    pub tx_i: usize
}

impl SessionItem for RTL8139 {
    fn scheme(&self) -> String {
        return "network".to_string();
    }

    fn open(&mut self, url: &URL) -> Box<Resource> {
        return RTL8139Resource::new(self);
    }

    fn on_irq(&mut self, irq: u8){
        if irq == self.irq {
            unsafe {
                let base = self.base as u16;

                loop {
                    let interrupt_status = inw(base + 0x3E);
                    outw(base + 0x3E, interrupt_status);

                    dh(interrupt_status as usize);
                    dl();

                    if interrupt_status == 0 {
                        break;
                    }
                }

                self.receive_inbound();
                self.send_outbound();
            }
        }
    }
}

impl RTL8139 {
    pub unsafe fn receive_inbound(&mut self) {
        let reenable = start_no_ints();

        let base = self.base as u16;

        let receive_buffer = ind(base + 0x30) as usize;
        let mut capr = (inw(base + 0x38) + 16) as usize;
        let cbr = inw(base + 0x3A) as usize;

        while capr != cbr {
            let frame_addr = receive_buffer + capr + 4;
            let frame_len = *((receive_buffer + capr + 2) as *const u16) as usize;

            for resource in self.resources.iter() {
                (**resource).inbound.push(Vec::from_raw_buf(frame_addr as *const u8, frame_len - 4));
            }

            capr = capr + frame_len + 4;
            capr = (capr + 3) & (0xFFFFFFFF - 3);
            if capr >= 8192 {
                capr -= 8192
            }

            outw(base + 0x38, (capr as u16) - 16);
        }

        end_no_ints(reenable);
    }

    pub unsafe fn send_outbound(&mut self) {
        let reenable = start_no_ints();

        let mut has_outbound = false;
        for resource in self.resources.iter() {
            if (**resource).outbound.len() > 0 {
                has_outbound = true;
            }
        }

        if has_outbound {
            let base = self.base as u16;

            loop {
                if ind(base + 0x10 + (self.tx_i as u16) * 4) & (1 << 13) == (1 << 13) {
                    let mut found = false;

                    for resource in self.resources.iter() {
                        if ! found {
                            match (**resource).outbound.pop() {
                                Option::Some(bytes) => {
                                    if bytes.len() < 8192 {
                                        found = true;

                                        d("Send ");
                                        dd(self.tx_i);
                                        d(": ");
                                        dd(bytes.len());
                                        dl();

                                        let tx_buffer = ind(base + 0x20 + (self.tx_i as u16) * 4);
                                        ::memcpy(tx_buffer as *mut u8, bytes.as_ptr(), bytes.len());

                                        outd(base + 0x20 + (self.tx_i as u16) * 4, tx_buffer);
                                        outd(base + 0x10 + (self.tx_i as u16) * 4, bytes.len() as u32 & 0x1FFF);
                                    }else{
                                        dl();
                                        d("RTL8139: Frame too long for transmit: ");
                                        dd(bytes.len());
                                        dl();
                                    }
                                },
                                Option::None => continue
                            }
                        }
                    }

                    if found {
                        self.tx_i = (self.tx_i + 1) % 4;
                    }else{
                        break;
                    }
                }else{
                    break;
                }
            }
        }

        end_no_ints(reenable);
    }

    pub unsafe fn init(&self){
        d("RTL8139 on: ");
        dh(self.base);
        if self.memory_mapped {
            d(" memory mapped");
        }else{
            d(" port mapped");
        }
        d(" IRQ: ");
        dbh(self.irq);

        pci_write(self.bus, self.slot, self.func, 0x04, pci_read(self.bus, self.slot, self.func, 0x04) | (1 << 2)); // Bus mastering

        let base = self.base as u16;

        outb(base + 0x52, 0);

        outb(base + 0x37, 0x10);
        while inb(base + 0x37) & 0x10 != 0 {}

        let receive_buffer = alloc(10240);
        outd(base + 0x30, receive_buffer as u32);
        d(" RBSTART: ");
        dh(ind(base + 0x30) as usize);

        for i in 0..4 {
            outd(base + 0x20 + (i as u16) * 4, alloc(8192) as u32);
        }

        outw(base + 0x3C, 5);
        d(" IMR: ");
        dh(inw(base + 0x3C) as usize);

        outb(base + 0x37, 0xC);
        d(" CMD: ");
        dbh(inb(base + 0x37));

        outd(base + 0x44, 0x8F);
        d(" RCR: ");
        dh(ind(base + 0x44) as usize);

        d(" MAC: ");
        let mac_low = ind(base);
        let mac_high = ind(base + 4);
        let mac = MACAddr{
            bytes: [
                mac_low as u8,
                (mac_low >> 8) as u8,
                (mac_low >> 16) as u8,
                (mac_low >> 24) as u8,
                mac_high as u8,
                (mac_high >> 8) as u8
            ]
        };
        mac.d();

        dl();
    }
}
