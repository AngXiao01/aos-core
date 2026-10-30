//! Persistent hash-array mapped trie. Cloning the map clones one `Rc` (O(1)).

use alloc::rc::Rc;
use alloc::vec::Vec;
use core::fmt::Debug;

const BITS: u32 = 5;
const MAX_SHIFT: u32 = 60;

pub trait HamtKey: Clone + Eq + Debug {
    fn hamt_hash(&self) -> u64;
}

fn fnv1a(bytes: &[u8]) -> u64 {
    let mut h = 0xcbf29ce484222325u64;
    for &b in bytes {
        h ^= b as u64;
        h = h.wrapping_mul(0x100000001b3);
    }
    h
}

impl HamtKey for aos_types::ObjectId {
    fn hamt_hash(&self) -> u64 {
        fnv1a(&self.0)
    }
}

impl HamtKey for alloc::string::String {
    fn hamt_hash(&self) -> u64 {
        fnv1a(self.as_bytes())
    }
}

#[derive(Clone)]
enum Node<K, V> {
    Leaf {
        hash: u64,
        key: K,
        value: V,
    },
    Collision {
        hash: u64,
        items: Vec<(K, V)>,
    },
    Branch {
        bitmap: u32,
        children: Vec<Rc<Node<K, V>>>,
    },
}

#[derive(Clone)]
pub struct Hamt<K, V> {
    root: Option<Rc<Node<K, V>>>,
    len: usize,
}

impl<K, V> Default for Hamt<K, V> {
    fn default() -> Self {
        Self::new()
    }
}

impl<K, V> Hamt<K, V> {
    pub fn new() -> Self {
        Self { root: None, len: 0 }
    }

    pub fn len(&self) -> usize {
        self.len
    }

    pub fn is_empty(&self) -> bool {
        self.len == 0
    }
}

impl<K: HamtKey, V: Clone> Hamt<K, V> {
    pub fn get(&self, key: &K) -> Option<&V> {
        let root = self.root.as_ref()?;
        get_node(root, 0, key.hamt_hash(), key)
    }

    pub fn insert(&self, key: K, value: V) -> Self {
        let hash = key.hamt_hash();
        match &self.root {
            None => Self {
                root: Some(Rc::new(Node::Leaf { hash, key, value })),
                len: 1,
            },
            Some(root) => {
                let (node, delta) = insert_node(root, 0, hash, key, value);
                Self {
                    root: Some(node),
                    len: (self.len as isize + delta) as usize,
                }
            }
        }
    }

    pub fn remove(&self, key: &K) -> Self {
        let Some(root) = &self.root else {
            return self.clone();
        };
        match remove_node(root, 0, key.hamt_hash(), key) {
            Remove::Missing => self.clone(),
            Remove::Empty => Self::new(),
            Remove::Node(node) => Self {
                root: Some(node),
                len: self.len - 1,
            },
        }
    }

    pub fn for_each(&self, f: &mut dyn FnMut(&K, &V)) {
        if let Some(root) = &self.root {
            walk(root, f);
        }
    }

    pub fn keys(&self) -> Vec<K> {
        let mut out = Vec::with_capacity(self.len);
        self.for_each(&mut |k, _| out.push(k.clone()));
        out
    }
}

fn slot(hash: u64, shift: u32) -> u32 {
    ((hash >> shift) & 31) as u32
}

fn bit(i: u32) -> u32 {
    1u32 << i
}

fn index_of(bitmap: u32, i: u32) -> usize {
    (bitmap & (bit(i) - 1)).count_ones() as usize
}

fn get_node<'a, K: Eq, V>(node: &'a Node<K, V>, shift: u32, hash: u64, key: &K) -> Option<&'a V> {
    match node {
        Node::Leaf { key: k, value, .. } => {
            if k == key {
                Some(value)
            } else {
                None
            }
        }
        Node::Collision { items, .. } => items.iter().find(|(k, _)| k == key).map(|(_, v)| v),
        Node::Branch { bitmap, children } => {
            let s = slot(hash, shift);
            if bitmap & bit(s) == 0 {
                return None;
            }
            let child = &children[index_of(*bitmap, s)];
            get_node(child, shift + BITS, hash, key)
        }
    }
}

fn insert_node<K: HamtKey, V: Clone>(
    node: &Node<K, V>,
    shift: u32,
    hash: u64,
    key: K,
    value: V,
) -> (Rc<Node<K, V>>, isize) {
    match node {
        Node::Leaf {
            hash: h,
            key: k,
            value: v,
        } => {
            if k == &key {
                return (Rc::new(Node::Leaf { hash, key, value }), 0);
            }
            if *h == hash || shift > MAX_SHIFT {
                return (
                    Rc::new(Node::Collision {
                        hash: *h,
                        items: alloc::vec![(k.clone(), v.clone()), (key, value)],
                    }),
                    1,
                );
            }
            let a = Rc::new(node.clone());
            let b = Rc::new(Node::Leaf { hash, key, value });
            (join(shift, *h, a, hash, b), 1)
        }
        Node::Collision { hash: h, items } => {
            let mut items = items.clone();
            if let Some(pos) = items.iter().position(|(k, _)| k == &key) {
                items[pos] = (key, value);
                return (Rc::new(Node::Collision { hash: *h, items }), 0);
            }
            items.push((key, value));
            (Rc::new(Node::Collision { hash: *h, items }), 1)
        }
        Node::Branch { bitmap, children } => {
            let s = slot(hash, shift);
            if bitmap & bit(s) == 0 {
                let leaf = Rc::new(Node::Leaf { hash, key, value });
                let idx = index_of(*bitmap, s);
                let mut children = children.clone();
                children.insert(idx, leaf);
                (
                    Rc::new(Node::Branch {
                        bitmap: bitmap | bit(s),
                        children,
                    }),
                    1,
                )
            } else {
                let idx = index_of(*bitmap, s);
                let (child, delta) = insert_node(&children[idx], shift + BITS, hash, key, value);
                let mut children = children.clone();
                children[idx] = child;
                (
                    Rc::new(Node::Branch {
                        bitmap: *bitmap,
                        children,
                    }),
                    delta,
                )
            }
        }
    }
}

fn join<K, V>(
    shift: u32,
    hash_a: u64,
    a: Rc<Node<K, V>>,
    hash_b: u64,
    b: Rc<Node<K, V>>,
) -> Rc<Node<K, V>> {
    if shift > MAX_SHIFT {
        return a;
    }
    let sa = slot(hash_a, shift);
    let sb = slot(hash_b, shift);
    if sa != sb {
        let (bitmap, children) = if sa < sb {
            (bit(sa) | bit(sb), alloc::vec![a, b])
        } else {
            (bit(sa) | bit(sb), alloc::vec![b, a])
        };
        Rc::new(Node::Branch { bitmap, children })
    } else {
        let child = join(shift + BITS, hash_a, a, hash_b, b);
        Rc::new(Node::Branch {
            bitmap: bit(sa),
            children: alloc::vec![child],
        })
    }
}

enum Remove<K, V> {
    Missing,
    Empty,
    Node(Rc<Node<K, V>>),
}

fn remove_node<K: HamtKey, V: Clone>(
    node: &Node<K, V>,
    shift: u32,
    hash: u64,
    key: &K,
) -> Remove<K, V> {
    match node {
        Node::Leaf { key: k, .. } => {
            if k == key {
                Remove::Empty
            } else {
                Remove::Missing
            }
        }
        Node::Collision { hash: h, items } => {
            let Some(pos) = items.iter().position(|(k, _)| k == key) else {
                return Remove::Missing;
            };
            if items.len() == 1 {
                return Remove::Empty;
            }
            if items.len() == 2 {
                let (k, v) = if pos == 0 {
                    items[1].clone()
                } else {
                    items[0].clone()
                };
                return Remove::Node(Rc::new(Node::Leaf {
                    hash: k.hamt_hash(),
                    key: k,
                    value: v,
                }));
            }
            let mut items = items.clone();
            items.remove(pos);
            Remove::Node(Rc::new(Node::Collision { hash: *h, items }))
        }
        Node::Branch { bitmap, children } => {
            let s = slot(hash, shift);
            if bitmap & bit(s) == 0 {
                return Remove::Missing;
            }
            let idx = index_of(*bitmap, s);
            match remove_node(&children[idx], shift + BITS, hash, key) {
                Remove::Missing => Remove::Missing,
                Remove::Empty => {
                    if children.len() == 1 {
                        return Remove::Empty;
                    }
                    let mut children = children.clone();
                    children.remove(idx);
                    let bitmap = bitmap & !bit(s);
                    if children.len() == 1 {
                        if let Node::Leaf { .. } | Node::Collision { .. } = children[0].as_ref() {
                            return Remove::Node(Rc::clone(&children[0]));
                        }
                    }
                    Remove::Node(Rc::new(Node::Branch { bitmap, children }))
                }
                Remove::Node(child) => {
                    let mut children = children.clone();
                    children[idx] = child;
                    Remove::Node(Rc::new(Node::Branch {
                        bitmap: *bitmap,
                        children,
                    }))
                }
            }
        }
    }
}

fn walk<K, V>(node: &Node<K, V>, f: &mut dyn FnMut(&K, &V)) {
    match node {
        Node::Leaf { key, value, .. } => f(key, value),
        Node::Collision { items, .. } => {
            for (k, v) in items {
                f(k, v);
            }
        }
        Node::Branch { children, .. } => {
            for c in children {
                walk(c, f);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use aos_types::ObjectId;

    #[test]
    fn insert_get_and_structural_share() {
        let mut m = Hamt::<ObjectId, u32>::new();
        let mut ids = Vec::new();
        for i in 0..200 {
            let id = ObjectId::from_parts(i, i as u128);
            ids.push(id);
            m = m.insert(id, i as u32);
        }
        assert_eq!(m.len(), 200);
        let forked = m.clone();
        let extra = ObjectId::from_parts(999, 1);
        let m2 = forked.insert(extra, 7);
        assert!(m.get(&extra).is_none());
        assert_eq!(m2.get(&extra), Some(&7));
        assert_eq!(m.get(&ids[10]), Some(&10));
        let m3 = m2.remove(&ids[10]);
        assert!(m3.get(&ids[10]).is_none());
        assert_eq!(m.get(&ids[10]), Some(&10));
        assert_eq!(m3.len(), 200);
    }

    #[test]
    fn collision_chain() {
        #[derive(Clone, Debug, PartialEq, Eq)]
        struct K(u8);
        impl HamtKey for K {
            fn hamt_hash(&self) -> u64 {
                1
            }
        }
        let mut m = Hamt::new();
        m = m.insert(K(1), "a");
        m = m.insert(K(2), "b");
        m = m.insert(K(3), "c");
        assert_eq!(m.get(&K(2)), Some(&"b"));
        m = m.insert(K(2), "z");
        assert_eq!(m.get(&K(2)), Some(&"z"));
        assert_eq!(m.len(), 3);
        m = m.remove(&K(1));
        assert!(m.get(&K(1)).is_none());
        assert_eq!(m.len(), 2);
    }
}
