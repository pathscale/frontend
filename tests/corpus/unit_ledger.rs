// Corpus unit: ledger. A struct mutated through `&mut self`, a `u64` total fed by `as` casts,
// a `match` over integer ranges, a `loop` with `break`, a tuple returned and destructured.
// `Q0` is the per-unit tag; see `unit_geometry.rs`.

pub struct LedgerQ0 {
    pub credit: u64,
    pub debit: u64,
    pub entries: u32,
}

pub enum EntryQ0 {
    Credit(u32),
    Debit(u32),
    Note,
}

impl LedgerQ0 {
    pub fn new() -> LedgerQ0 {
        LedgerQ0 { credit: 0, debit: 0, entries: 0 }
    }

    pub fn record(&mut self, entry: &EntryQ0) {
        match entry {
            EntryQ0::Credit(n) => self.credit = self.credit + (*n as u64),
            EntryQ0::Debit(n) => self.debit = self.debit + (*n as u64),
            EntryQ0::Note => {}
        }
        self.entries = self.entries + 1;
    }

    pub fn balance(&self) -> (u64, bool) {
        if self.credit < self.debit {
            (self.debit - self.credit, false)
        } else {
            (self.credit - self.debit, true)
        }
    }

    pub fn is_empty(&self) -> bool {
        self.entries == 0
    }
}

pub fn entry_for_Q0(i: u32) -> EntryQ0 {
    match i {
        0 => EntryQ0::Note,
        1..=4 => EntryQ0::Credit(i * 10),
        5..=7 => EntryQ0::Debit(i * 3),
        _ => EntryQ0::Credit(i),
    }
}

pub fn fill_Q0(count: u32) -> LedgerQ0 {
    let mut ledger = LedgerQ0::new();
    let mut i: u32 = 0;
    loop {
        if i == count {
            break;
        }
        let entry = entry_for_Q0(i);
        ledger.record(&entry);
        i = i + 1;
    }
    ledger
}

pub fn audit_Q0(count: u32) -> u64 {
    let ledger = fill_Q0(count);
    let (amount, positive) = ledger.balance();
    if positive && ledger.entries > 0 { amount } else { 0 }
}

pub fn compare_Q0(a: u32, b: u32) -> u32 {
    let first = audit_Q0(a);
    let second = audit_Q0(b);
    if first == second {
        0
    } else if first < second {
        1
    } else {
        2
    }
}
