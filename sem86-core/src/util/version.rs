#[derive(Debug)]
pub struct Versioner(u16);

impl Default for Versioner {
    fn default() -> Self {
        Self::new()
    }
}

impl Versioner {
    pub fn new() -> Self {
        Self(1)
    }

    #[inline(always)]
    pub fn current_version(&self) -> Version {
        Version(self.0)
    }

    /// Returns true if the version number wrapped around.
    #[must_use]
    pub fn increment(&mut self) -> bool {
        if let Some(next) = self.0.checked_add(1) {
            self.0 = next;
            false
        } else {
            self.0 = 1;
            true
        }
    }
}

#[derive(Copy, Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct Version(u16);

impl Version {
    /// Version that is always smaller than any version given out by a [`Versioner`].
    pub const ZERO: Version = Version(0);

    pub fn update(&mut self, current: Version) {
        self.0 = current.0;
    }
}
