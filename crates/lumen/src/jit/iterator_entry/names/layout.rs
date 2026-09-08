//! Owner-probed scope layout for helper-free walks of live lexical ancestors.
use crate::interpreter::{Env, Scope, VarMap};
use std::cell::RefCell;
use std::mem::{align_of, offset_of, size_of};
use std::rc::Rc;
use std::sync::OnceLock;

/// Scope offsets are relative to `Rc::as_ptr(env)`, not the stored Rc allocation pointer.
/// A consumer must check `valid`, reject negative borrow counts and `under_with`, and keep
/// the starting environment strongly rooted throughout the callback-free walk.
pub(crate) struct ScopeNativeLayout {
    pub(crate) valid: bool,
    pub(crate) borrow_flag: usize,
    pub(crate) parent: usize,
    pub(crate) under_with: usize,
    pub(crate) generation: usize,
    /// Add this to a nonnull stored `Option<Env>` word to obtain `Rc::as_ptr(parent)`.
    pub(crate) rc_data_offset: usize,
}

pub(crate) fn layout() -> &'static ScopeNativeLayout {
    static LAYOUT: OnceLock<ScopeNativeLayout> = OnceLock::new();
    LAYOUT.get_or_init(probe)
}

fn probe() -> ScopeNativeLayout {
    let env = crate::interpreter::new_scope(None);
    let base = Rc::as_ptr(&env) as usize;
    let value = env.as_ptr() as usize;
    let value_offset = value.wrapping_sub(base);
    let word = size_of::<usize>();
    let pointer_sized = size_of::<Env>() == word && size_of::<Option<Env>>() == word;
    let (rc_data_offset, niche_valid) = if pointer_sized {
        // Rc and Option<Rc> have the documented nonnull-pointer niche; no padding is read.
        let some = Some(env.clone());
        let none: Option<Env> = None;
        let stored = unsafe { (&env as *const Env).cast::<usize>().read() };
        let some_word = unsafe { (&some as *const Option<Env>).cast::<usize>().read() };
        let none_word = unsafe { (&none as *const Option<Env>).cast::<usize>().read() };
        let adjustment = base.wrapping_sub(stored);
        (
            adjustment,
            stored == some_word && none_word == 0 && adjustment < 256,
        )
    } else {
        (0, false)
    };
    ScopeNativeLayout {
        valid: word == 8
            && niche_valid
            && value_offset == word
            && align_of::<RefCell<Scope>>() >= align_of::<isize>()
            && borrow_word_valid(),
        borrow_flag: 0,
        parent: value_offset + offset_of!(Scope, parent),
        under_with: value_offset + offset_of!(Scope, under_with),
        generation: value_offset + offset_of!(Scope, vars) + VarMap::generation_offset(),
        rc_data_offset,
    }
}

fn borrow_word_valid() -> bool {
    // Probe the compact RefCell<()> representation only: its sole nonzero-sized field is
    // the initialized borrow counter. In particular, never scan padding in RefCell<Scope>.
    // A std build with extra borrow diagnostics or a different prefix fails closed.
    if size_of::<RefCell<()>>() != size_of::<isize>()
        || align_of::<RefCell<()>>() != align_of::<isize>()
    {
        return false;
    }
    let cell = RefCell::new(());
    let read = || unsafe { (&cell as *const RefCell<()>).cast::<isize>().read() };
    if read() != 0 {
        return false;
    }
    let first = cell.borrow();
    let first_ok = read() == 1;
    let second = cell.borrow();
    let second_ok = read() == 2;
    drop((first, second));
    let exclusive = cell.borrow_mut();
    let exclusive_ok = read() == -1;
    drop(exclusive);
    first_ok && second_ok && exclusive_ok && read() == 0
}
