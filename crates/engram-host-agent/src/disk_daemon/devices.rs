use super::DiskRole;

/// The NBD devices owned by one sandbox.
pub struct SandboxDisks<T> {
    pub expected: std::collections::BTreeSet<DiskRole>,
    pub root: T,
    pub swap: Option<T>,
}

impl<T> SandboxDisks<T> {
    pub fn iter(&self) -> impl Iterator<Item = (DiskRole, &T)> {
        std::iter::once((DiskRole::Root, &self.root))
            .chain(self.swap.as_ref().map(|state| (DiskRole::Swap, state)))
    }

    pub fn iter_mut(&mut self) -> impl Iterator<Item = (DiskRole, &mut T)> {
        std::iter::once((DiskRole::Root, &mut self.root))
            .chain(self.swap.as_mut().map(|state| (DiskRole::Swap, state)))
    }

    pub fn get(&self, role: DiskRole) -> Option<&T> {
        match role {
            DiskRole::Root => Some(&self.root),
            DiskRole::Swap => self.swap.as_ref(),
        }
    }

    pub fn get_mut(&mut self, role: DiskRole) -> Option<&mut T> {
        match role {
            DiskRole::Root => Some(&mut self.root),
            DiskRole::Swap => self.swap.as_mut(),
        }
    }

    pub fn into_devices(self) -> impl Iterator<Item = (DiskRole, T)> {
        std::iter::once((DiskRole::Root, self.root))
            .chain(self.swap.map(|state| (DiskRole::Swap, state)))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn root_first_iteration_and_role_lookup() {
        let mut disks = SandboxDisks {
            expected: [engram_core::DiskRole::Root].into_iter().collect(),
            root: 1,
            swap: Some(2),
        };
        assert_eq!(
            disks.iter().map(|(r, d)| (r, *d)).collect::<Vec<_>>(),
            vec![(DiskRole::Root, 1), (DiskRole::Swap, 2)]
        );
        for (_, value) in disks.iter_mut() {
            *value += 1;
        }
        assert_eq!(disks.get(DiskRole::Root), Some(&2));
        assert_eq!(disks.get(DiskRole::Swap), Some(&3));
        disks.swap = None;
        assert_eq!(disks.iter().count(), 1);
        assert_eq!(disks.get(DiskRole::Swap), None);
    }
}
