//! The order a function's blocks are emitted in, and the loops nested over it. Blocks
//! are sorted so that every jump goes forward except loop back edges, and each natural
//! loop's blocks are contiguous with its header first: the emitter then turns forward
//! jumps into branches out of nested blocks and back edges into branches to a wasm
//! `loop`, with no dispatch in between. Functions whose control flow is irreducible
//! keep the address order, where earlier blocks are reached through the dispatcher.

use crate::cfg::{Cfg, Term};
use crate::fasthash::HashMap;

/// The emission structure over positions in `Layout::order`.
pub enum Item {
    Block(u32),
    /// A loop: the first item is its header block.
    Loop(Vec<Item>),
}

impl Item {
    /// The position of the block this item starts with.
    pub fn first(&self) -> u32 {
        match self {
            Item::Block(p) => *p,
            Item::Loop(items) => items[0].first(),
        }
    }
}

pub struct Layout {
    /// Block keys in emission order.
    pub order: Vec<u32>,
    pub items: Vec<Item>,
}

/// Blocks of the function this block's terminator may continue at (a fall-through
/// last, so the sort tends to place it next).
pub fn successors(cfg: &Cfg, key: u32) -> Vec<u32> {
    let block = &cfg.blocks[&key];
    let mut v = match &block.term {
        Term::Fall(n) | Term::Jump(n) => vec![*n],
        Term::Cond { taken, next } => vec![*taken, *next],
        Term::Table(targets) => targets.clone(),
        Term::InlineCall { entry } => vec![*entry],
        Term::InlineRet { ret_key, .. } | Term::InlineTail { ret_key, .. } => vec![*ret_key],
        Term::Ret => cfg.ret_targets.clone(),
        Term::TailCall(_) | Term::IndirectTail | Term::Trap | Term::Bad(_) => Vec::new(),
    };
    v.retain(|t| cfg.blocks.contains_key(t));
    v
}

pub fn layout(cfg: &Cfg) -> Layout {
    structured(cfg).unwrap_or_else(|| {
        let order: Vec<u32> = cfg.blocks.keys().copied().collect();
        let items = (0..order.len() as u32).map(Item::Block).collect();
        Layout { order, items }
    })
}

/// The sorted layout, or None when some cycle is entered other than through a block
/// that dominates it.
fn structured(cfg: &Cfg) -> Option<Layout> {
    let keys: Vec<u32> = cfg.blocks.keys().copied().collect();
    let n = keys.len();
    let id: HashMap<u32, usize> = keys.iter().enumerate().map(|(i, &k)| (k, i)).collect();
    let succ: Vec<Vec<usize>> = keys
        .iter()
        .map(|&k| {
            let mut v: Vec<usize> = Vec::new();
            for t in successors(cfg, k) {
                let t = id[&t];
                if !v.contains(&t) {
                    v.push(t);
                }
            }
            v
        })
        .collect();
    let entry = *id.get(&cfg.entry)?;

    // reverse postorder of the blocks reachable from the entry
    let mut post = Vec::with_capacity(n);
    let mut seen = vec![false; n];
    let mut stack = vec![(entry, 0usize)];
    seen[entry] = true;
    while let Some((u, i)) = stack.last_mut() {
        let u = *u;
        if *i < succ[u].len() {
            let v = succ[u][*i];
            *i += 1;
            if !seen[v] {
                seen[v] = true;
                stack.push((v, 0));
            }
        } else {
            post.push(u);
            stack.pop();
        }
    }
    let rpo: Vec<usize> = post.into_iter().rev().collect();
    let mut rpo_num = vec![usize::MAX; n];
    for (i, &u) in rpo.iter().enumerate() {
        rpo_num[u] = i;
    }
    let mut preds: Vec<Vec<usize>> = vec![Vec::new(); n];
    for &u in &rpo {
        for &v in &succ[u] {
            preds[v].push(u);
        }
    }

    // dominators (Cooper, Harvey and Kennedy)
    let mut idom = vec![usize::MAX; n];
    idom[entry] = entry;
    let intersect = |idom: &[usize], mut a: usize, mut b: usize| {
        while a != b {
            while rpo_num[a] > rpo_num[b] {
                a = idom[a];
            }
            while rpo_num[b] > rpo_num[a] {
                b = idom[b];
            }
        }
        a
    };
    loop {
        let mut changed = false;
        for &b in rpo.iter().skip(1) {
            let mut new = usize::MAX;
            for &p in &preds[b] {
                if idom[p] != usize::MAX {
                    new = if new == usize::MAX { p } else { intersect(&idom, p, new) };
                }
            }
            if idom[b] != new {
                idom[b] = new;
                changed = true;
            }
        }
        if !changed {
            break;
        }
    }
    let dominates = |h: usize, mut u: usize| loop {
        if u == h {
            return true;
        }
        if u == entry {
            return false;
        }
        u = idom[u];
    };

    // back edges (to a block that dominates the source) and their natural loops; any
    // other edge to an earlier block makes the graph irreducible
    let mut back: Vec<Vec<bool>> = succ.iter().map(|s| vec![false; s.len()]).collect();
    let mut header_loop = vec![usize::MAX; n];
    let mut bodies: Vec<(usize, Vec<usize>)> = Vec::new();
    let mut sources: Vec<Vec<usize>> = Vec::new();
    for &u in &rpo {
        for (k, &v) in succ[u].iter().enumerate() {
            if rpo_num[v] > rpo_num[u] {
                continue;
            }
            if !dominates(v, u) {
                return None;
            }
            back[u][k] = true;
            if header_loop[v] == usize::MAX {
                header_loop[v] = bodies.len();
                bodies.push((v, vec![v]));
                sources.push(Vec::new());
            }
            sources[header_loop[v]].push(u);
        }
    }
    // each body: what reaches a back edge's source without passing the header
    let mut stamp = vec![usize::MAX; n];
    for (l, (h, body)) in bodies.iter_mut().enumerate() {
        stamp[*h] = l;
        let mut work = std::mem::take(&mut sources[l]);
        while let Some(x) = work.pop() {
            if stamp[x] != l {
                stamp[x] = l;
                body.push(x);
                work.extend(preds[x].iter().copied());
            }
        }
    }
    // innermost loop of each block and each loop's parent: natural loops with
    // different headers are nested or disjoint, so going from larger to smaller
    // leaves each block with the smallest one holding it
    let mut by_size: Vec<usize> = (0..bodies.len()).collect();
    by_size.sort_by_key(|&l| std::cmp::Reverse(bodies[l].1.len()));
    let mut loop_of = vec![usize::MAX; n];
    let mut parent = vec![usize::MAX; bodies.len()];
    for &l in &by_size {
        parent[l] = loop_of[bodies[l].0];
        for &x in &bodies[l].1 {
            loop_of[x] = l;
        }
    }
    let within = |l: usize, x: usize| {
        let mut m = loop_of[x];
        while m != usize::MAX {
            if m == l {
                return true;
            }
            m = parent[m];
        }
        false
    };

    // Topological order of the forward edges, by reverse postorder, finishing the
    // innermost loop entered before taking blocks outside it.
    let mut fwd_preds = vec![0u32; n];
    for &u in &rpo {
        for (k, &v) in succ[u].iter().enumerate() {
            if !back[u][k] {
                fwd_preds[v] += 1;
            }
        }
    }
    let mut remaining: Vec<usize> = bodies.iter().map(|b| b.1.len()).collect();
    let mut ready = vec![entry];
    let mut open: Vec<usize> = Vec::new();
    let mut sorted = Vec::with_capacity(n);
    while sorted.len() < rpo.len() {
        while open.last().is_some_and(|&l| remaining[l] == 0) {
            open.pop();
        }
        let top = open.last().copied();
        let pick = ready
            .iter()
            .enumerate()
            .filter(|&(_, &x)| top.is_none_or(|l| within(l, x)))
            .min_by_key(|&(_, &x)| rpo_num[x])
            .map(|(i, _)| i)?;
        let b = ready.swap_remove(pick);
        if header_loop[b] != usize::MAX {
            open.push(header_loop[b]);
        }
        sorted.push(b);
        let mut m = loop_of[b];
        while m != usize::MAX {
            remaining[m] -= 1;
            m = parent[m];
        }
        for (k, &v) in succ[b].iter().enumerate() {
            if !back[b][k] {
                fwd_preds[v] -= 1;
                if fwd_preds[v] == 0 {
                    ready.push(v);
                }
            }
        }
    }
    // blocks the entry does not reach, in address order
    sorted.extend((0..n).filter(|&x| !seen[x]));

    // nest the loops over the order
    fn nest(sorted: &[usize], at: &mut usize, cur: usize, loop_of: &[usize], parent: &[usize], header_loop: &[usize], within: &dyn Fn(usize, usize) -> bool) -> Option<Vec<Item>> {
        let mut items = Vec::new();
        while *at < sorted.len() {
            let x = sorted[*at];
            if cur != usize::MAX && !within(cur, x) {
                break;
            }
            // the outermost loop holding x inside `cur`
            let mut child = usize::MAX;
            let mut m = loop_of[x];
            while m != usize::MAX && m != cur {
                child = m;
                m = parent[m];
            }
            *at += 1;
            if child == usize::MAX {
                items.push(Item::Block(*at as u32 - 1));
            } else {
                if header_loop[x] != child {
                    return None;
                }
                let mut inner = vec![Item::Block(*at as u32 - 1)];
                inner.extend(nest(sorted, at, child, loop_of, parent, header_loop, within)?);
                items.push(Item::Loop(inner));
            }
        }
        Some(items)
    }
    let mut at = 0;
    let items = nest(&sorted, &mut at, usize::MAX, &loop_of, &parent, &header_loop, &within)?;
    if at != sorted.len() {
        return None;
    }
    Some(Layout { order: sorted.iter().map(|&x| keys[x]).collect(), items })
}
