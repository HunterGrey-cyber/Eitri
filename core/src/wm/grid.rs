//! Pure allocation and child ordering policies used by the module grid.

pub mod order;
pub mod throttle;

use std::collections::BTreeSet;

use crate::layout::{Layout, LayoutError, ModuleId, Size};

pub use crate::layout::DEFAULT_STRIP_PX;

/// The content size the geometry is computed against before the grid's first allocation: the
/// default window (1280x760) minus the top bar, 39px by its CSS (`tree::BELOW_ROOT_SHARE` has the
/// arithmetic; not measured). Only a key pressed before the window is on screen could see it --
/// and `main.rs`'s hiding of the bottom terminal in the initial layout, which reads no geometry at
/// all unless the terminal held the keys, which at startup nothing does.
pub const UNALLOCATED: Size = Size { w: 1280, h: 721 };

/// The size the geometry is computed against: the grid's allocation, or [`UNALLOCATED`] while GTK
/// still reports none (0x0 until the first allocation).
pub fn frame_size(width: i32, height: i32) -> Size {
    if width > 0 && height > 0 {
        Size { w: width, h: height }
    } else {
        UNALLOCATED
    }
}

/// Whether giving `id` the keys raises it (`Layout::set_focus`): a float not on top of the z-order.
pub fn would_raise(layout: &Layout, id: &ModuleId) -> bool {
    layout.is_floating(id) && layout.floats().last().map(|float| &float.id) != Some(id)
}

/// The floats on screen when nothing is zoomed, bottom first: the layout's floats that are not hidden,
/// as `arrange` lists them. What the float band holds; a zoom unmaps floats but does not change it.
pub fn shown_floats(layout: &Layout) -> Vec<ModuleId> {
    layout
        .floats()
        .iter()
        .filter(|float| layout.is_shown(&float.id))
        .map(|float| float.id.clone())
        .collect()
}

/// Whether the grid restacks when applying the layout: the set of floats on screen (`floats`, in
/// z-order) is not the set the last restack put in the float band (`band`). The z-order alone never
/// counts -- a raise restacks by itself, and must not make every layout application move children.
pub fn floats_need_restack(band: &BTreeSet<ModuleId>, floats: &[ModuleId]) -> bool {
    band.len() != floats.len() || floats.iter().any(|id| !band.contains(id))
}

/// A grid child as the restack sees it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Slot {
    Host(ModuleId),
    Strip(usize),
    Handle(usize),
    /// A piece of float chrome: its bar, then its grip, always together.
    Chrome(usize),
}

/// One reorder: put `slot` right after `after` (`None`: first child).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SlotMove {
    pub slot: Slot,
    pub after: Option<Slot>,
}

/// The reorders that take `children` to the band order with `floats` (bottom first) floating, chrome `i`
/// drawing `chrome[i]`: hosts that do not float keep their relative order below every strip; each
/// float host goes to the top in z-order, followed by its chrome; chrome drawing nothing goes last.
///
/// **What moves.** Strips, handles and every host in the host band that stays tiled keep their places:
/// a host counts as in the host band when it lies below the first child that is not a host. Only float
/// hosts, hosts above that point that no longer float (they go back to the top of the host band, right
/// below the first strip or handle), and chrome are named. So a host tiled both before and after is
/// never named, and neither is the editor, which never floats and is the first host the window adds.
///
/// **Why the moves give the order.** Each named slot is put right after its predecessor in the target
/// order, in that order: no later move puts anything between the two (only the slot that follows a
/// predecessor in the target goes after it), and the slots not named keep their relative order. A slot
/// already right after its predecessor is not named, so the result restacks to no moves at all.
pub fn restack_moves(children: &[Slot], floats: &[ModuleId], chrome: &[ModuleId]) -> Vec<SlotMove> {
    let floating = |id: &ModuleId| floats.contains(id);
    let host_band_end = children
        .iter()
        .position(|slot| !matches!(slot, Slot::Host(_)))
        .unwrap_or(children.len());
    let named: Vec<&Slot> = children
        .iter()
        .enumerate()
        .filter(|(k, slot)| match slot {
            Slot::Host(id) => floating(id) || (*id != ModuleId::editor() && *k > host_band_end),
            Slot::Chrome(_) => true,
            Slot::Strip(_) | Slot::Handle(_) => false,
        })
        .map(|(_, slot)| slot)
        .collect();
    let kept: Vec<&Slot> = children.iter().filter(|slot| !named.contains(slot)).collect();
    let returning = named
        .iter()
        .filter(|slot| matches!(slot, Slot::Host(id) if !floating(id)))
        .copied();
    let split = kept
        .iter()
        .position(|slot| !matches!(slot, Slot::Host(_)))
        .unwrap_or(kept.len());

    let mut target: Vec<Slot> = kept[..split].iter().map(|s| (*s).clone()).collect();
    target.extend(returning.cloned());
    target.extend(kept[split..].iter().map(|s| (*s).clone()));
    for id in floats {
        let host = Slot::Host(id.clone());
        if children.contains(&host) {
            target.push(host);
        }
        if let Some(i) = chrome.iter().position(|c| c == id) {
            if children.contains(&Slot::Chrome(i)) {
                target.push(Slot::Chrome(i));
            }
        }
    }
    for slot in children {
        if matches!(slot, Slot::Chrome(_)) && !target.contains(slot) {
            target.push(slot.clone());
        }
    }
    debug_assert_eq!(target.len(), children.len(), "the target is a reordering: {target:?}");

    let mut now: Vec<Slot> = children.to_vec();
    let mut moves = Vec::new();
    for (k, slot) in target.iter().enumerate() {
        if !named.contains(&slot) {
            continue;
        }
        let after = k.checked_sub(1).map(|p| target[p].clone());
        let at = now
            .iter()
            .position(|s| s == slot)
            .expect("every target slot is a child");
        let in_place = match &after {
            None => at == 0,
            Some(a) => at > 0 && now[at - 1] == *a,
        };
        if in_place {
            continue;
        }
        now.remove(at);
        let to = match &after {
            None => 0,
            Some(a) => now.iter().position(|s| s == a).expect("the predecessor is a child") + 1,
        };
        now.insert(to, slot.clone());
        moves.push(SlotMove {
            slot: slot.clone(),
            after,
        });
    }
    debug_assert_eq!(now, target, "the moves give the target");
    debug_assert!(
        moves.iter().all(|m| m.slot != Slot::Host(ModuleId::editor())),
        "the editor is never reordered"
    );
    moves
}

/// The layers of the grid's child list, bottom to top. GTK picks and paints in child-list order, so a
/// band higher up takes a press where its widget overlaps one lower down, and paints over it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Band {
    /// A module host: below everything else, so a strip or a divider at its edge gets the press.
    Host,
    /// A tabbed or stacked container's strip: above the hosts, whose rectangles it borders.
    Strip,
    /// A divider handle: above hosts and strips, so its edge columns are always draggable.
    Handle,
    /// A floating module's host, above every tiled thing (the grid restacks it there).
    FloatHost,
    /// A float's chrome, its bar and its grip, right after its float host.
    FloatBar,
}

impl Band {
    /// Lower is further down. A float host and its bar share the top rank: each bar is added right
    /// after its own host, which is then the last child, so both go to the end and stay paired.
    fn rank(self) -> u8 {
        match self {
            Band::Host => 0,
            Band::Strip => 1,
            Band::Handle => 2,
            Band::FloatHost | Band::FloatBar => 3,
        }
    }
}

/// Returns the index for band `new` before the first higher band in `children`, preserving insertion order
/// within each band. Hosts precede strips, strips precede handles, and handles precede floating
/// hosts. A floating host and its bar are appended together at the end.
///
/// GTK picks children from last to first (`gtk_widget_do_pick`). A widget contains both edges
/// of its border box: `gsk_rounded_rect_locate_point` excludes a point only beyond `x + width`.
/// A host ending where a divider begins therefore contains the divider's first pixel column.
/// The later child receives a press there, so a host appended after a handle can take its clicks.
/// Handles also paint after every host and strip.
pub fn band_insert_index(children: &[Band], new: Band) -> usize {
    children
        .iter()
        .position(|band| band.rank() > new.rank())
        .unwrap_or(children.len())
}

/// Whether `module` may be placed into the tree: only a module this window has a host for.
/// A leaf without a host leaves an empty rectangle and can receive focus without a surface.
pub fn placeable(hosted: &[ModuleId], module: &ModuleId) -> Result<(), LayoutError> {
    if hosted.contains(module) {
        Ok(())
    } else {
        Err(LayoutError::NotInTree(module.clone()))
    }
}

/// How the arrangement's `strips` strips are drawn by a pool of `pool` strip widgets: how many new
/// widgets to make first, and for each strip in arrangement order, the pool slot that draws it. The
/// pool grows only when a pass needs more strips than it has ever drawn at once; slots past the
/// last strip are hidden.
pub fn strip_assignments(pool: usize, strips: usize) -> (usize, Vec<usize>) {
    (strips.saturating_sub(pool), (0..strips).collect())
}

#[cfg(test)]
mod tests {
    use super::throttle::WebThrottle;
    use super::*;
    use crate::layout::{arrange, covering, Arrangement, Axis, FloatRect, Frame, Node};

    /// One child of the grid, in the model below.
    #[derive(Debug, Clone, PartialEq)]
    enum Child {
        Host(ModuleId),
        /// The n-th handle the grid made: it draws `arrangement.dividers[n]` (`place_handles`).
        Handle(usize),
    }

    /// The child list the grid builds for hosts added in this order: each host where
    /// [`band_insert_index`] puts a host, then (from the second host on) one handle where it puts a
    /// handle, as `new_handle` does.
    fn stack_as_add_builds(ids: &[ModuleId]) -> Vec<Child> {
        let band = |c: &Child| match c {
            Child::Host(_) => Band::Host,
            Child::Handle(_) => Band::Handle,
        };
        let mut children: Vec<Child> = Vec::new();
        for (i, id) in ids.iter().enumerate() {
            let bands: Vec<Band> = children.iter().map(band).collect();
            children.insert(band_insert_index(&bands, Band::Host), Child::Host(id.clone()));
            if i > 0 {
                let bands: Vec<Band> = children.iter().map(band).collect();
                children.insert(band_insert_index(&bands, Band::Handle), Child::Handle(i - 1));
            }
        }
        children
    }

    /// `children` after `moves`, each applied as `gtk_widget_insert_after` does: the slot leaves its
    /// place and goes right after `after` (first when `None`).
    fn apply_moves(children: &[Slot], moves: &[SlotMove]) -> Vec<Slot> {
        let mut now = children.to_vec();
        for m in moves {
            let at = now.iter().position(|s| *s == m.slot).expect("a move names a child");
            now.remove(at);
            let to = match &m.after {
                None => 0,
                Some(a) => now.iter().position(|s| s == a).expect("after names a child") + 1,
            };
            now.insert(to, m.slot.clone());
        }
        now
    }

    /// The band a slot is in once `floats` float.
    fn slot_band(slot: &Slot, floats: &[ModuleId]) -> Band {
        match slot {
            Slot::Host(id) if floats.contains(id) => Band::FloatHost,
            Slot::Host(_) => Band::Host,
            Slot::Strip(_) => Band::Strip,
            Slot::Handle(_) => Band::Handle,
            Slot::Chrome(_) => Band::FloatBar,
        }
    }

    /// What `restack` leaves, checked whole: the bands are monotone, the floats are on top in z-order,
    /// each followed by the chrome that draws it, and restacking again moves nothing.
    fn assert_restacked(result: &[Slot], floats: &[ModuleId], chrome: &[ModuleId], context: &str) {
        let bands: Vec<Band> = result.iter().map(|s| slot_band(s, floats)).collect();
        assert!(
            bands.windows(2).all(|w| w[0].rank() <= w[1].rank()),
            "{context}: out of band order: {result:?}"
        );
        let mut last = None;
        for id in floats {
            let Some(at) = result.iter().position(|s| *s == Slot::Host(id.clone())) else {
                continue;
            };
            assert!(
                last.is_none_or(|l| l < at),
                "{context}: {id} out of z-order: {result:?}"
            );
            last = Some(at);
            if let Some(i) = chrome.iter().position(|c| c == id) {
                assert_eq!(
                    result.get(at + 1),
                    Some(&Slot::Chrome(i)),
                    "{context}: {id}'s chrome right after it: {result:?}"
                );
            }
        }
        assert_eq!(
            restack_moves(result, floats, chrome),
            [],
            "{context}: a second restack moves nothing"
        );
    }

    /// A small xorshift, so the mixed lists below are the same on every run.
    struct Xorshift(u64);

    impl Xorshift {
        fn next(&mut self, below: usize) -> usize {
            self.0 ^= self.0 << 13;
            self.0 ^= self.0 >> 7;
            self.0 ^= self.0 << 17;
            (self.0 % below as u64) as usize
        }
    }

    /// Floats `terminal` at `rect` over the default window with the terminal shown and the keys on
    /// `keys`.
    fn with_a_float(rect: FloatRect, keys: &ModuleId) -> Layout {
        let mut layout = crate::wm::persist::first_launch_layout(&[]).unwrap();
        layout.show(&ModuleId::terminal()).unwrap();
        let frame = Frame::new(UNALLOCATED, 1);
        assert_eq!(
            crate::layout::float_at(&mut layout, &ModuleId::terminal(), Some(rect), &frame),
            Ok(true)
        );
        layout.set_focus(keys).unwrap();
        layout
    }

    /// A key pressed before the window is laid out (`Ctrl+a h` in the first frame, a shim letter
    /// racing the first allocation) still meets a real geometry, not a 0x0 one in which every
    /// module is an empty rectangle and nothing is anyone's neighbour.
    #[test]
    fn before_the_first_allocation_the_geometry_uses_the_default_windows_content() {
        assert_eq!(frame_size(0, 0), UNALLOCATED);
        assert_eq!(frame_size(1280, 0), UNALLOCATED, "one axis unallocated is unallocated");
        assert_eq!(frame_size(-1, 400), UNALLOCATED);
        assert_eq!(frame_size(900, 500), Size { w: 900, h: 500 });
    }

    /// Every host sits below every handle however many modules there are, and the hosts keep the
    /// order they were added in (focus and HINT never read the child list, but nothing should
    /// reorder the modules as a side effect).
    #[test]
    fn hosts_stack_below_every_handle_in_the_order_they_were_added() {
        let ids: Vec<ModuleId> = [ModuleId::editor(), ModuleId::agent()]
            .into_iter()
            .chain(["a", "b", "c"].map(ModuleId::lua))
            .collect();
        let children = stack_as_add_builds(&ids);
        let hosts: Vec<Child> = ids.iter().cloned().map(Child::Host).collect();
        let handles: Vec<Child> = (0..ids.len() - 1).map(Child::Handle).collect();
        assert_eq!(children, [hosts, handles].concat());
        assert_eq!(
            band_insert_index(&[], Band::Host),
            0,
            "the first host, into an empty grid"
        );
    }

    /// Every add puts its child at its band's place, whatever was added before it and in whatever
    /// order: hosts, then strips, then handles, then each float host followed by its own chrome, and
    /// inside each band the order things were added in. Every interleaving of five adds over hosts,
    /// strips, handles and floats (a float host with its chrome added right after it) is replayed on a
    /// grid with no float, one and two floats already restacked into the float band, so a new host,
    /// strip or handle added while floats exist is checked to land below the first float host.
    #[test]
    fn the_band_order_holds_for_every_add_while_floats_exist() {
        // What each add brings: a host, a strip, a handle, or a float host and then its chrome.
        const KINDS: [&[Band]; 4] = [
            &[Band::Host],
            &[Band::Strip],
            &[Band::Handle],
            &[Band::FloatHost, Band::FloatBar],
        ];
        const ADDS: u32 = 5;
        // The grid before the adds: the editor, the chat and a handle between them, then each float
        // already present followed by its chrome, as `restack` leaves them. A float's adds are
        // numbered from 100 so they never compare below the adds replayed after them.
        let before = |floats: usize| -> Vec<(Band, usize)> {
            let mut children = vec![(Band::Host, 0), (Band::Host, 0), (Band::Handle, 0)];
            for f in 0..floats {
                children.push((Band::FloatHost, 100 + f));
                children.push((Band::FloatBar, 100 + f));
            }
            children
        };
        for present in 0..=2 {
            for mut code in 0..KINDS.len().pow(ADDS) {
                // Each child is its band and the number of the add that made it.
                let mut children = before(present);
                let mut order = Vec::new();
                for n in 1..=ADDS as usize {
                    let kind = KINDS[code % KINDS.len()];
                    code /= KINDS.len();
                    order.push(kind[0]);
                    let n = if kind[0] == Band::FloatHost { 200 + n } else { n };
                    for &band in kind {
                        let bands: Vec<Band> = children.iter().map(|(b, _)| *b).collect();
                        let first_float = bands.iter().position(|b| *b == Band::FloatHost);
                        let at = band_insert_index(&bands, band);
                        if band != Band::FloatHost && band != Band::FloatBar {
                            if let Some(first_float) = first_float {
                                assert!(
                                    at <= first_float,
                                    "{present} floats, {order:?}: a {band:?} above the first float host"
                                );
                            }
                        }
                        children.insert(at, (band, n));
                    }
                }
                let bands: Vec<Band> = children.iter().map(|(b, _)| *b).collect();
                assert!(
                    bands.windows(2).all(|w| w[0].rank() <= w[1].rank()),
                    "{present} floats, {order:?}: out of band order: {bands:?}"
                );
                for band in [Band::Host, Band::Strip, Band::Handle, Band::FloatHost] {
                    let adds: Vec<usize> = children.iter().filter(|(b, _)| *b == band).map(|(_, n)| *n).collect();
                    assert!(
                        adds.windows(2).all(|w| w[0] <= w[1]),
                        "{present} floats, {order:?}: {band:?}s out of the order they were added in: {adds:?}"
                    );
                }
                for (i, (band, n)) in children.iter().enumerate() {
                    if *band == Band::FloatHost {
                        assert_eq!(
                            children.get(i + 1),
                            Some(&(Band::FloatBar, *n)),
                            "{present} floats, {order:?}: a float host's chrome sits right after it: {children:?}"
                        );
                    }
                }
            }
        }
    }

    #[test]
    fn adding_a_host_after_its_float_was_applied_restacks_it_above_every_tile() {
        let terminal = ModuleId::terminal();
        let agent = ModuleId::agent();
        let floats = vec![terminal.clone(), agent.clone()];
        let chrome = floats.clone();
        let mut children = vec![
            Slot::Host(ModuleId::editor()),
            Slot::Handle(0),
            Slot::Host(agent.clone()),
            Slot::Chrome(0),
            Slot::Chrome(1),
        ];
        children = apply_moves(&children, &restack_moves(&children, &floats, &chrome));
        let float_band: BTreeSet<_> = floats.iter().cloned().collect();
        assert!(!floats_need_restack(&float_band, &floats));
        // The arriving host is parented below the handles, then add's restack restores its z position.
        children.insert(1, Slot::Host(terminal.clone()));
        assert!(float_band.contains(&terminal));
        let result = apply_moves(&children, &restack_moves(&children, &floats, &chrome));
        assert_eq!(
            result,
            vec![
                Slot::Host(ModuleId::editor()),
                Slot::Handle(0),
                Slot::Host(terminal),
                Slot::Chrome(0),
                Slot::Host(agent),
                Slot::Chrome(1)
            ]
        );
        assert_restacked(&result, &floats, &chrome, "a float host added late");
    }

    /// An unhosted `canvas` or a misspelled `lua:` id is refused, as a module
    /// that is not in the layout is refused everywhere else, rather than placed as a leaf with no host.
    #[test]
    fn only_a_module_with_a_host_can_be_placed() {
        let hosted = [
            ModuleId::editor(),
            ModuleId::agent(),
            ModuleId::terminal(),
            ModuleId::lua("notes"),
        ];
        for id in &hosted {
            assert_eq!(placeable(&hosted, id), Ok(()));
        }
        for id in [ModuleId::parse("canvas").unwrap(), ModuleId::lua("nots")] {
            assert_eq!(placeable(&hosted, &id), Err(LayoutError::NotInTree(id.clone())));
        }
    }

    /// From thousands of child lists -- tiled hosts in add order with the editor first, strips and
    /// handles, the floats of a previous restack each with its chrome, idle chrome, and chrome made for
    /// this restack at the end -- to a new set of floats in a new z-order: no move names the editor or
    /// a host tiled before and after, tiled hosts keep their order, and the result is restacked.
    #[test]
    fn restacking_moves_only_float_hosts_and_their_chrome_never_the_editor() {
        let modules = [
            ModuleId::editor(),
            ModuleId::agent(),
            ModuleId::terminal(),
            ModuleId::parse("terminal#2").unwrap(),
            ModuleId::lua("a"),
            ModuleId::lua("b"),
        ];
        let mut rng = Xorshift(0x9e37_79b9_7f4a_7c15);
        // A random subset of everything but the editor, in a random order.
        let pick_floats = |rng: &mut Xorshift| {
            let mut floats: Vec<ModuleId> = Vec::new();
            for _ in 0..rng.next(4) {
                let id = modules[1 + rng.next(modules.len() - 1)].clone();
                if !floats.contains(&id) {
                    floats.push(id);
                }
            }
            floats
        };
        for case in 0..5000 {
            let hosts = 1 + rng.next(modules.len());
            let present: Vec<ModuleId> = modules[..hosts].to_vec();
            let was: Vec<ModuleId> = pick_floats(&mut rng)
                .into_iter()
                .filter(|id| present.contains(id))
                .collect();
            let drew = was.clone();
            let idle = rng.next(2);
            let strips = rng.next(3);
            let mut children: Vec<Slot> = present
                .iter()
                .filter(|id| !was.contains(id))
                .map(|id| Slot::Host(id.clone()))
                .collect();
            children.extend((0..strips).map(Slot::Strip));
            children.extend((0..hosts - 1).map(Slot::Handle));
            for id in &was {
                children.push(Slot::Host(id.clone()));
                children.push(Slot::Chrome(drew.iter().position(|d| d == id).unwrap()));
            }
            children.extend((drew.len()..drew.len() + idle).map(Slot::Chrome));

            let floats: Vec<ModuleId> = pick_floats(&mut rng)
                .into_iter()
                .filter(|id| present.contains(id))
                .collect();
            // The chrome order the grid's owner rule gives: floats that kept their slot, then new ones
            // in z-order. `restack_moves` takes whatever list it is handed; this one keeps the cases
            // close to what the grid really passes.
            let mut chrome: Vec<_> = drew.iter().filter(|id| floats.contains(id)).cloned().collect();
            chrome.extend(floats.iter().filter(|id| !drew.contains(id)).cloned());
            // `restack` grows the pool first; a new piece goes in at the end, as `insert_in_band` puts it.
            let pool = drew.len() + idle;
            children.extend((pool..chrome.len().max(pool)).map(Slot::Chrome));
            let context = format!("case {case}: {children:?} from floats {was:?} to {floats:?}");

            let moves = restack_moves(&children, &floats, &chrome);
            for m in &moves {
                assert_ne!(m.slot, Slot::Host(ModuleId::editor()), "{context}: the editor moved");
                assert!(
                    !matches!(&m.slot, Slot::Host(id) if !was.contains(id) && !floats.contains(id)),
                    "{context}: a host tiled before and after moved: {m:?}"
                );
                assert!(
                    !matches!(m.slot, Slot::Strip(_) | Slot::Handle(_)),
                    "{context}: a strip or a handle moved: {m:?}"
                );
            }
            let result = apply_moves(&children, &moves);
            assert_restacked(&result, &floats, &chrome, &context);
            let tiled = |list: &[Slot]| -> Vec<Slot> {
                list.iter()
                    .filter(|s| matches!(s, Slot::Host(id) if !was.contains(id) && !floats.contains(id)))
                    .cloned()
                    .collect()
            };
            assert_eq!(
                tiled(&result),
                tiled(&children),
                "{context}: tiled hosts kept their order"
            );
        }
    }

    /// One module floating in one workspace and tiled in the other: each switch moves its host between
    /// the float band and the host band -- back right below the first strip, then up again -- and the
    /// editor is never named.
    #[test]
    fn a_module_floating_in_one_workspace_and_tiled_in_the_other_changes_band_on_switch() {
        let (editor, agent, terminal) = (ModuleId::editor(), ModuleId::agent(), ModuleId::terminal());
        let mut children: Vec<Slot> = stack_as_add_builds(&[editor.clone(), agent.clone(), terminal.clone()])
            .into_iter()
            .map(|c| match c {
                Child::Host(id) => Slot::Host(id),
                Child::Handle(n) => Slot::Handle(n),
            })
            .collect();
        children.insert(3, Slot::Strip(0));
        assert_eq!(
            children,
            [
                Slot::Host(editor.clone()),
                Slot::Host(agent.clone()),
                Slot::Host(terminal.clone()),
                Slot::Strip(0),
                Slot::Handle(0),
                Slot::Handle(1),
            ],
            "the premise"
        );

        // The workspace where the terminal floats: its host goes on top, then its chrome, made now.
        let floating = [terminal.clone()];
        let chrome = floating.to_vec();
        children.push(Slot::Chrome(0));
        let moves = restack_moves(&children, &floating, &chrome);
        assert_eq!(
            moves,
            [SlotMove {
                slot: Slot::Host(terminal.clone()),
                after: Some(Slot::Handle(1)),
            }]
        );
        let up = apply_moves(&children, &moves);
        assert_restacked(&up, &floating, &chrome, "floating");

        // The workspace where it is tiled: back right below the first strip.
        let chrome_tiled = Vec::new();
        let moves = restack_moves(&up, &[], &chrome_tiled);
        assert_eq!(
            moves,
            [SlotMove {
                slot: Slot::Host(terminal.clone()),
                after: Some(Slot::Host(agent.clone())),
            }]
        );
        let down = apply_moves(&up, &moves);
        assert_eq!(down, children, "back where it was, the idle chrome last");
        assert_restacked(&down, &[], &chrome_tiled, "tiled");

        // And up again.
        let chrome = floating.to_vec();
        let moves = restack_moves(&down, &floating, &chrome);
        assert!(moves.iter().all(|m| m.slot != Slot::Host(editor.clone())));
        assert_eq!(apply_moves(&down, &moves), up);
    }

    /// `swap.marked` between a float and a tile: the tile's module now floats and the float's is
    /// tiled. Both hosts change band -- the new float's to the top, followed by chrome 0, which drew the
    /// old one; the old float's back below the first strip or handle -- and the editor is never named.
    /// The chrome list is the one the grid's owner rule gives for this swap (the freed slot 0 goes to the
    /// new float); that rule lives with the chrome pool in the shell and is tested there.
    #[test]
    fn a_swap_marked_between_a_float_and_a_tile_moves_both_hosts_between_bands() {
        let (editor, agent, two) = (
            ModuleId::editor(),
            ModuleId::agent(),
            ModuleId::parse("terminal#2").unwrap(),
        );
        let children = [
            Slot::Host(editor.clone()),
            Slot::Host(agent.clone()),
            Slot::Handle(0),
            Slot::Host(two.clone()),
            Slot::Chrome(0),
        ];
        let floats = [agent.clone()];
        let chrome = [agent.clone()];
        let moves = restack_moves(&children, &floats, &chrome);
        assert!(moves.iter().all(|m| m.slot != Slot::Host(editor.clone())), "{moves:?}");
        let result = apply_moves(&children, &moves);
        assert_eq!(
            result,
            [
                Slot::Host(editor.clone()),
                Slot::Host(two.clone()),
                Slot::Handle(0),
                Slot::Host(agent.clone()),
                Slot::Chrome(0),
            ]
        );
        assert_restacked(&result, &floats, &chrome, "after the swap");
    }

    /// The predicate `apply` restacks on: a changed set of floats on screen, never the z-order alone
    /// (a raise restacks by itself) and never an unchanged set.
    #[test]
    fn the_floating_set_decides_when_apply_restacks() {
        let (a, b, c) = (ModuleId::agent(), ModuleId::terminal(), ModuleId::lua("c"));
        let band: BTreeSet<ModuleId> = [a.clone(), b.clone()].into_iter().collect();
        assert!(!floats_need_restack(&band, &[a.clone(), b.clone()]), "unchanged");
        assert!(!floats_need_restack(&band, &[b.clone(), a.clone()]), "another z-order");
        assert!(
            floats_need_restack(&band, std::slice::from_ref(&a)),
            "one sank or was stowed"
        );
        assert!(
            floats_need_restack(&band, &[a.clone(), b.clone(), c.clone()]),
            "one more floats"
        );
        assert!(floats_need_restack(&band, &[a.clone(), c.clone()]), "a swap");
        assert!(!floats_need_restack(&BTreeSet::new(), &[]), "nothing floats");
    }

    /// Giving the keys to a float under another raises it, and what the floats cover changes with
    /// it: a terminal float raised over the floating chat covers it now. That is the raise the change
    /// hooks must hear of (`restacked_after_raise`); the float already on top, a tile, or the editor
    /// raises nothing.
    #[test]
    fn giving_the_keys_to_a_lower_float_raises_it_over_what_it_overlaps() {
        let terminal = ModuleId::terminal();
        let chat = ModuleId::agent();
        let rect = FloatRect {
            cx: 0.5,
            cy: 0.5,
            w: 400,
            h: 300,
        };
        let frame = Frame::new(UNALLOCATED, 1);
        let mut layout = with_a_float(rect, &terminal);
        assert_eq!(
            crate::layout::float_at(&mut layout, &chat, Some(rect), &frame),
            Ok(true)
        );
        layout.set_focus(&chat).unwrap();
        assert!(
            covering(&arrange(&layout, &frame), &chat).is_empty(),
            "the chat floats on top"
        );
        assert!(would_raise(&layout, &terminal));
        assert!(!would_raise(&layout, &chat), "on top already");
        assert!(!would_raise(&layout, &ModuleId::editor()), "a tile is never raised");

        layout.set_focus(&terminal).unwrap();
        assert_eq!(covering(&arrange(&layout, &frame), &chat), vec![terminal.clone()]);
        assert!(!would_raise(&layout, &terminal));
    }

    #[test]
    fn strip_assignments_grow_the_pool_only_when_needed() {
        assert_eq!(strip_assignments(0, 0), (0, vec![]));
        assert_eq!(strip_assignments(0, 2), (2, vec![0, 1]), "the first two containers");
        assert_eq!(
            strip_assignments(2, 2),
            (0, vec![0, 1]),
            "the same two again: nothing new"
        );
        assert_eq!(
            strip_assignments(3, 1),
            (0, vec![0]),
            "fewer: the rest are hidden, none dropped"
        );
        assert_eq!(
            strip_assignments(1, 3),
            (2, vec![0, 1, 2]),
            "only the missing ones are made"
        );
        // Every slot named exists once the new ones are made, and no slot draws two strips.
        for pool in 0..5 {
            for strips in 0..5 {
                let (create, slots) = strip_assignments(pool, strips);
                assert_eq!(slots.len(), strips);
                assert!(slots.iter().all(|&s| s < pool + create));
                let mut seen = slots.clone();
                seen.dedup();
                assert_eq!(seen.len(), slots.len());
                assert_eq!(create, strips.saturating_sub(pool));
            }
        }
    }
    /// After unzooming, a held web allocation must not cover its settled sibling. Picking follows
    /// each child's allocation, so a paint clip alone cannot keep the sibling's clicks safe.
    #[test]
    fn a_zoomed_then_unzoomed_web_host_never_steals_a_click_from_its_settled_neighbour() {
        let (agent, editor) = (ModuleId::agent(), ModuleId::editor());
        let root = Node::split(Axis::Row, 0.5, Node::Leaf(agent.clone()), Node::Leaf(editor.clone()));
        let mut layout = Layout::new(root, agent.clone()).unwrap();
        let frame = Frame::new(Size { w: 1280, h: 721 }, 1);
        let mut throttle = WebThrottle::default();

        // The agent is zoomed to the whole window, and settles there once quiet.
        layout.toggle_zoom(&agent);
        let zoomed_target = arrange(&layout, &frame).rect_of(&agent).unwrap();
        let window = Size { w: 1280, h: 721 };
        throttle.allocate(&agent, zoomed_target, false, window);
        throttle.settle();
        let real_alloc_while_zoomed = throttle.allocate(&agent, zoomed_target, true, window).rect;
        assert_eq!(real_alloc_while_zoomed, zoomed_target);

        // Unzoom: the agent goes back to its left column, the editor takes the right.
        layout.unzoom();
        let arrangement = arrange(&layout, &frame);
        let (agent_target, editor_target) = (
            arrangement.rect_of(&agent).unwrap(),
            arrangement.rect_of(&editor).unwrap(),
        );
        assert!(editor_target.w > 0, "editor is shown again, unzoomed");

        let held = throttle.allocate(&agent, agent_target, true, window);
        assert!(held.held, "the target changed: the throttle holds the agent's old size");
        assert!(
            held.rect.x + held.rect.w <= agent_target.x + agent_target.w,
            "the held allocation {:?} reaches past the agent's target {agent_target:?} toward the editor",
            held.rect
        );

        // What `main.rs` really allocates: the editor gets its own (unthrottled) target, the agent
        // whatever the throttle just returned.
        let real_arrangement = Arrangement {
            floats: Vec::new(),
            modules: vec![(editor.clone(), editor_target), (agent.clone(), held.rect)],
            dividers: Vec::new(),
            ..Default::default()
        };
        let children = stack_as_add_builds(&[editor.clone(), agent.clone()]);
        let inside_editor = (
            editor_target.x + editor_target.w / 2,
            editor_target.y + editor_target.h / 2,
        );
        assert_eq!(
            children
                .iter()
                .rev()
                .find(|child| match child {
                    Child::Host(id) => real_arrangement.rect_of(id).is_some_and(|r| {
                        inside_editor.0 >= r.x
                            && inside_editor.0 <= r.x + r.w
                            && inside_editor.1 >= r.y
                            && inside_editor.1 <= r.y + r.h
                    }),
                    Child::Handle(_) => false,
                })
                .cloned(),
            Some(Child::Host(editor)),
            "a click inside the editor's own drawn area must not resolve to the agent's stale \
             allocation {:?} (editor's real area: {editor_target:?})",
            held.rect,
        );
    }
}
