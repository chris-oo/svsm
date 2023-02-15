// SPDX-License-Identifier: (GPL-2.0-or-later OR MIT)
//
// Copyright (c) 2022 SUSE LLC
//
// Author: Joerg Roedel <jroedel@suse.de>
//
// vim: ts=4 sw=4 et

extern crate alloc;

use super::gdt::load_tss;
use super::tss::{X86Tss, IST_DF};
use crate::cpu::tss::TSS_LIMIT;
use crate::mm::{SVSM_PERCPU_BASE, SVSM_STACKS_INIT_TASK, SVSM_PERCPU_VMSA_BASE,
    SVSM_STACK_IST_DF_BASE, SVSM_PERCPU_CAA_BASE, virt_to_phys, phys_to_virt};
use crate::mm::alloc::{allocate_page, allocate_zeroed_page};
use crate::mm::stack::{allocate_stack_addr, stack_base_pointer};
use crate::mm::pagetable::{PageTable, PageTableRef, get_init_pgtable_locked};
use crate::sev::ghcb::GHCB;
use crate::sev::utils::RMPFlags;
use crate::sev::vmsa::{allocate_new_vmsa, free_vmsa, VMSASegment, VMSA};
use crate::types::{PhysAddr, VirtAddr};
use crate::types::{SVSM_TR_FLAGS, SVSM_TSS};
use crate::cpu::vmsa::init_guest_vmsa;
use crate::utils::{page_align, page_offset};
use crate::locking::{SpinLock, LockGuard};
use alloc::vec::Vec;
use core::ptr;
use core::sync::atomic::{AtomicBool, Ordering};

struct PerCpuInfo {
    apic_id: u32,
    addr: VirtAddr,
}

impl PerCpuInfo {
    const fn new(apic_id: u32, addr: VirtAddr) -> Self {
        PerCpuInfo { apic_id: apic_id, addr: addr }
    }
}

// PERCPU areas virtual addresses into shared memory
static PERCPU_AREAS : SpinLock::<Vec::<PerCpuInfo>> = SpinLock::new(Vec::new());

#[derive(Copy, Clone)]
pub struct VmsaRef {
    pub vaddr: VirtAddr,
    pub paddr: PhysAddr,
    pub guest_owned : bool,
}

impl VmsaRef {
    const fn new(v: VirtAddr, p: PhysAddr, g: bool) -> Self {
        VmsaRef { vaddr: v, paddr: p, guest_owned: g }
    }

    pub fn vmsa(&self) -> &mut VMSA {
        let ptr: *mut VMSA = self.vaddr as *mut VMSA;
        unsafe { ptr.as_mut().unwrap() }
    }
}

struct IstStacks {
    double_fault_stack: Option<VirtAddr>,
}

impl IstStacks {
    const fn new() -> Self {
        IstStacks {
            double_fault_stack: None,
        }
    }
}

pub struct PerCpu {
    online: AtomicBool,
    apic_id: u32,
    pgtbl: SpinLock<PageTableRef>,
    ghcb: *mut GHCB,
    init_stack: Option<VirtAddr>,
    ist: IstStacks,
    tss: X86Tss,
    svsm_vmsa: Option<VmsaRef>,
    guest_vmsa: SpinLock::<Option<VmsaRef>>,
    caa_addr: Option<VirtAddr>,
    reset_ip: u64,
}

impl PerCpu {
    pub const fn new() -> Self {
        PerCpu {
            online: AtomicBool::new(false),
            apic_id: 0,
            pgtbl: SpinLock::<PageTableRef>::new(PageTableRef::unset()),
            ghcb: ptr::null_mut(),
            init_stack: None,
            ist: IstStacks::new(),
            tss: X86Tss::new(),
            svsm_vmsa: None,
            guest_vmsa: SpinLock::new(None),
            caa_addr: None,
            reset_ip: 0xffff_fff0u64,
        }
    }

    pub fn alloc(apic_id: u32) -> Result<*mut PerCpu, ()> {
        let vaddr = allocate_zeroed_page()?;

        PERCPU_AREAS.lock().push(PerCpuInfo::new(apic_id, vaddr));

        unsafe {
            let percpu: *mut PerCpu = vaddr as *mut PerCpu;
            (*percpu) = PerCpu::new();
            Ok(percpu)
        }
    }

    pub fn set_online(&mut self) {
        self.online.store(true, Ordering::Relaxed);
    }

    pub fn is_online(&self) -> bool {
        self.online.load(Ordering::Acquire)
    }

    pub fn set_apic_id(&mut self, apic_id: u32) {
        self.apic_id = apic_id;
    }

    pub const fn get_apic_id(&self) -> u32 {
        self.apic_id
    }

    fn allocate_page_table(&mut self) -> Result<(), ()> {
        let pgtable_ref = get_init_pgtable_locked().clone_shared()?;
        let mut pgtbl = self.pgtbl.lock();
        *pgtbl = pgtable_ref;
        Ok(())
    }

    pub fn set_pgtable(&mut self, pgtable : PageTableRef) {
        let mut my_pgtable = self.pgtbl.lock();
        *my_pgtable = pgtable;
    }

    fn allocate_init_stack(&mut self) -> Result<(), ()> {
        allocate_stack_addr(SVSM_STACKS_INIT_TASK, &mut self.get_pgtable()).
            expect("Failed to allocate per-cpu init stack");
        self.init_stack = Some(SVSM_STACKS_INIT_TASK);
        Ok(())
    }

    fn allocate_ist_stacks(&mut self) -> Result<(), ()> {
        allocate_stack_addr(SVSM_STACK_IST_DF_BASE, &mut self.get_pgtable())
            .expect("Failed to allocate percpu double-fault stack");

        self.ist.double_fault_stack = Some(SVSM_STACK_IST_DF_BASE);
        Ok(())
    }

    pub fn get_pgtable(&self) -> LockGuard<PageTableRef> {
        self.pgtbl.lock()
    }

    pub fn setup_ghcb(&mut self) -> Result<(), ()> {
        let ghcb_page = allocate_page().expect("Failed to allocate GHCB page");
        self.ghcb = ghcb_page as *mut GHCB;
        unsafe { (*self.ghcb).init() }
    }

    pub fn register_ghcb(&self) -> Result<(), ()> {
        unsafe { self.ghcb.as_ref().unwrap().register() }
    }

    pub fn get_top_of_stack(&self) -> VirtAddr {
        stack_base_pointer(self.init_stack.unwrap())
    }

    fn setup_tss(&mut self) {
        self.tss.ist_stacks[IST_DF] = stack_base_pointer(self.ist.double_fault_stack.unwrap());
    }

    pub fn map_self(&mut self) -> Result<(), ()> {
        let vaddr = (self as *const PerCpu) as VirtAddr;
        let paddr = virt_to_phys(vaddr);
        let flags = PageTable::data_flags();

        self.get_pgtable().map_4k(SVSM_PERCPU_BASE, paddr, &flags)
    }

    pub fn setup(&mut self) -> Result<(), ()> {
        // Allocate page-table
        self.allocate_page_table()?;

        // Map PerCpu data in own page-table
        self.map_self()?;

        // Setup GHCB
        self.setup_ghcb()?;

        // Allocate per-cpu init stack
        self.allocate_init_stack()?;

        // Allocate IST stacks
        self.allocate_ist_stacks()?;

        // Setup TSS
        self.setup_tss();

        Ok(())
    }

    // Setup code which needs to run on the target CPU
    pub fn setup_on_cpu(&self) -> Result<(), ()> {
        self.register_ghcb()
    }

    pub fn load_pgtable(&mut self) {
        self.get_pgtable().load();
    }

    pub fn load_tss(&mut self) {
        load_tss(&self.tss);
    }

    pub fn load(&mut self) {
        self.load_pgtable();
        self.load_tss();
    }

    pub fn shutdown(&mut self) -> Result<(), ()> {
        if self.ghcb == ptr::null_mut() {
            return Ok(());
        }

        unsafe { (*self.ghcb).shutdown() }
    }

    pub fn set_reset_ip(&mut self, reset_ip: u64) {
        self.reset_ip = reset_ip;
    }

    pub fn ghcb(&mut self) -> &'static mut GHCB {
        unsafe { self.ghcb.as_mut().unwrap() }
    }

    pub fn alloc_svsm_vmsa(&mut self) -> Result<(), ()> {
        if let Some(_) = self.svsm_vmsa {
            return Err(());
        }

        let vaddr = allocate_new_vmsa(RMPFlags::VMPL1)?;
        let paddr = virt_to_phys(vaddr);

        self.svsm_vmsa = Some(VmsaRef::new(vaddr, paddr, false));

        Ok(())
    }

    pub fn get_svsm_vmsa(&mut self) -> &mut Option<VmsaRef> {
        &mut self.svsm_vmsa
    }

    pub fn prepare_svsm_vmsa(&mut self, start_rip: u64) {
        let vmsa = self.svsm_vmsa.unwrap();

		vmsa.vmsa().tr = self.vmsa_tr_segment();
		vmsa.vmsa().rip = start_rip;
		vmsa.vmsa().rsp = self.get_top_of_stack().try_into().unwrap();
		vmsa.vmsa().cr3 = self.get_pgtable().cr3_value().try_into().unwrap();
    }

    fn unmap_guest_vmsa_locked(&self, vmsa_ref: &mut Option<VmsaRef>) -> Result<(), ()> {
        let new_ref = vmsa_ref.clone();
        if let Some(vmsa_info) = new_ref {
            self.get_pgtable().unmap_4k(vmsa_info.vaddr)?;

            if !vmsa_info.guest_owned {
                // Get the virtual address in SVSM shared memory
                let vaddr = phys_to_virt(vmsa_info.paddr);
                free_vmsa(vaddr);
            }
        }

        *vmsa_ref = None;

        Ok(())
    }

    pub fn unmap_guest_vmsa(&self) -> Result<(), ()> {
        let mut vmsa_ref = self.guest_vmsa.lock();

        self.unmap_guest_vmsa_locked(&mut vmsa_ref)
    }

    pub fn map_guest_vmsa(&self, paddr: PhysAddr, guest_owned: bool) -> Result<(), ()> {
        let mut vmsa_ref = self.guest_vmsa.lock();
        self.unmap_guest_vmsa_locked(&mut vmsa_ref)?;

        let flags = PageTable::data_flags();
        let vaddr = SVSM_PERCPU_VMSA_BASE;

        self.get_pgtable().map_4k(vaddr, paddr, &flags)?;

        *vmsa_ref = Some(VmsaRef::new(SVSM_PERCPU_VMSA_BASE, paddr, guest_owned));

        Ok(())
    }

    pub fn alloc_guest_vmsa(&self) -> Result<(), ()> {
        let vmsa = allocate_new_vmsa(RMPFlags::VMPL1)?;
        let phys = virt_to_phys(vmsa);   

        if let Err(_) = self.map_guest_vmsa(phys, false) {
            free_vmsa(vmsa);
        }

        Ok(())
    }

    pub fn get_guest_vmsa(&self) -> LockGuard<Option<VmsaRef>> {
        self.guest_vmsa.lock()
    }

    fn vmsa_tr_segment(&self) -> VMSASegment {
        VMSASegment {
            selector: SVSM_TSS,
            flags: SVSM_TR_FLAGS,
            limit: TSS_LIMIT as u32,
            base: (&self.tss as *const X86Tss) as u64,
        }
    }

    pub fn prepare_guest_vmsa(&mut self) -> Result<(),()> {
        let guard = self.get_guest_vmsa();
        let vmsa_ref = guard.clone();
        let vmsa = vmsa_ref.unwrap();

        init_guest_vmsa(vmsa.vmsa(), self.reset_ip);

        Ok(())
    }

    pub fn get_caa_addr(&self) -> Option<VirtAddr> {
        self.caa_addr
    }

    pub fn unmap_caa(&mut self) -> Result<(),()> {
        if let Some(v) = self.caa_addr {
            let start = page_align(v);

            self.caa_addr = None;
            self.get_pgtable().unmap_4k(start)?;
        }

        Ok(())
    }

    pub fn map_caa_phys(&mut self, paddr: PhysAddr) -> Result<(),()> {
        self.unmap_caa()?;

        let paddr_aligned = page_align(paddr);
        let page_offset = page_offset(paddr);
        let flags = PageTable::data_flags();

        let vaddr = SVSM_PERCPU_CAA_BASE;

        self.get_pgtable().map_4k(vaddr, paddr_aligned, &flags)?;

        self.caa_addr = Some(vaddr + page_offset);

        Ok(())
    }
}

unsafe impl Sync for PerCpu {}

pub fn this_cpu() -> &'static PerCpu {
    unsafe {
        let ptr = SVSM_PERCPU_BASE as *mut PerCpu;
        ptr.as_ref().unwrap()
    }
}

pub fn this_cpu_mut() -> &'static mut PerCpu {
    unsafe {
        let ptr = SVSM_PERCPU_BASE as *mut PerCpu;
        ptr.as_mut().unwrap()
    }
}

pub fn percpu(apic_id: u32) -> Option<&'static PerCpu> {
    for i in PERCPU_AREAS
        .lock()
        .iter()
        .filter(|x| (*x).apic_id == apic_id) {
            let ptr = (*i).addr as *const PerCpu;
            unsafe { return Some(ptr.as_ref().unwrap()); }
    }
    None
}

