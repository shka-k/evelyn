//! Bundled defaults compiled into the binary — built-in themes, the
//! built-in shader sources, and the named constants section `Default`
//! impls pull from. Anything that ships in-tree as "the out-of-box
//! experience" lives here so it's easy to audit in one place.

pub(super) mod defaults;
pub(super) mod shaders;
pub(super) mod themes;
