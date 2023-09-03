#![deny(warnings)]
#![forbid(unsafe_code)]
#![warn(missing_docs)]
#![warn(clippy::missing_docs_in_private_items)]

//! `modularity` is a bare-bones library for loading and linking [WebAssembly components](https://github.com/WebAssembly/component-model).
//! It serves as a foundation for WASM-based plugin and modding systems by providing the following functionality:
//!
//! - Resolving dependency graphs of WASM packages from arbitrary sources
//! - Instantiating WASM packages with imports from other components and the host
//! - Allowing the host to inspect and call package exports
//!
//! ## Usage
//!
//! The example below illustrates how to use this crate. A complete version may be found in [the examples folder](examples/).
//! It first creates a [`PackageResolver`], specifying the list of packages that the application desires to load.
//! Then, it repeatedly calls [`PackageResolver::resolve`], supplying new components whenever the resolver reports that it needs them.
//! Once the resolver has finished building the dependency graph, it produces a [`PackageContextImage`]. The image is subsequently applied
//! to the [`PackageContext`], where all of the components are linked and instantiated. After this, the package exports may be accessed through the context.
//!
//! ```ignore
//! // Create the WASM engine and store
//! let engine = Engine::new(wasmi::Engine::default());
//! let mut store = Store::new(&engine, ());
//!
//! // Create a context to hold packages
//! let mut ctx = PackageContext::default();
//!
//! // Create a resolver with the list of top-level dependencies
//! let mut resolver = Some(PackageResolver::new(package_ids), Linker::default());
//!
//! while let Some(r) = take(&mut resolver) {
//!     match r.resolve() {
//!         Ok(x) => {
//!             // Create a transition to move the context to the new set of packages
//!             // The linking process can be customized here
//!             let transition = PackageContextTransitionBuilder::new(&x, &ctx)
//!                 .build(&mut store, &ctx)
//!                 .unwrap();
//!
//!             // Apply the transition to the package context
//!             transition.apply(&mut store);
//!
//!             println!("Loaded packages are {:?}", ctx.packages().collect::<Vec<_>>());
//!         }
//!         Err(PackageResolverError::MissingPackages(mut r)) => {
//!             for u in r.unresolved() {
//!                 // Gets the component with the specified ID from a source
//!                 u.resolve(u.id(), get_package(&u));
//!             }
//!             resolver = Some(r);
//!         }
//!         x => panic!("Error occurred: {x:?}"),
//!     }
//! }
//! ```
//!
//! `modularity` relies on the [`wasm_component_layer`] crate for creating loaded WASM modules. It is the
//! responsibility of the consumer to supply parsed [`wasm_component_layer::Component`] instances from a source.

use anyhow::Error;
use anyhow::*;
use bitvec::access::*;
use bitvec::prelude::*;
use fxhash::*;
use ref_cast::*;
use semver::*;
use std::borrow::*;
use std::hash::*;
use std::mem::*;
use std::ops::*;
use std::sync::atomic::*;
use std::sync::*;
use topo_sort::*;
use wasm_component_layer::*;

/// A wrapper that can compare `Option<Version>`s, treating `None` like a wildcard
/// version that matches anything and is less than all other versions.
#[derive(PartialEq, Eq)]
struct PartialVersionRef<'a>(Option<&'a Version>);

impl<'a> PartialVersionRef<'a> {
    /// Determines whether the `semver` equation `other = ^self` is satisfied.
    pub fn matches(&self, other: &Self) -> bool {
        match (&self.0, &other.0) {
            (None, None) => true,
            (Some(_), None) => false,
            (None, Some(_)) => true,
            (Some(a), Some(b)) => a.major == b.major && a.minor == b.minor && a <= b,
        }
    }
}

impl<'a> PartialOrd for PartialVersionRef<'a> {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(match (&self.0, &other.0) {
            (None, None) => std::cmp::Ordering::Equal,
            (Some(_), None) => std::cmp::Ordering::Greater,
            (None, Some(_)) => std::cmp::Ordering::Less,
            (Some(a), Some(b)) => a.cmp(b),
        })
    }
}

impl<'a> Ord for PartialVersionRef<'a> {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        match (&self.0, &other.0) {
            (None, None) => std::cmp::Ordering::Equal,
            (Some(_), None) => std::cmp::Ordering::Greater,
            (None, Some(_)) => std::cmp::Ordering::Less,
            (Some(a), Some(b)) => a.cmp(b),
        }
    }
}

/// A package that consists of an identifier and parsed binary.
#[derive(Clone, Debug)]
struct ResolvedPackage {
    /// The ID of the package.
    pub id: PackageIdentifier,
    /// The parsed component.
    pub component: Component,
}

/// Stores transient information about a package during version resolution.
#[derive(Clone, Debug)]
struct PackageResolutionInfo {
    /// The maximum version of the package loaded so far.
    pub version: Option<Version>,
    /// Whether any candidate packages must match in version exactly.
    pub exact: bool,
    /// Whether this package is resolved and in the component graph.
    pub state: PackageResolutionState,
}

/// Denotes a package's current standing in the component graph.
#[derive(Clone, Debug)]
enum PackageResolutionState {
    /// The package has a pending resolution request with the given index.
    Unresolved(u16),
    /// The package has been resolved to a component.
    Resolved(Component),
    /// The package has been resolved to a component, and a vertex representing
    /// it has been added to the graph.
    InGraph(Component),
}

/// An interface identifier which is equal across all versions.
#[derive(Clone, Debug, RefCast)]
#[repr(transparent)]
struct UnversionedInterface(InterfaceIdentifier);

impl PartialEq for UnversionedInterface {
    fn eq(&self, other: &Self) -> bool {
        self.0.name() == other.0.name() && self.0.package().name() == other.0.package().name()
    }
}

impl Eq for UnversionedInterface {}

impl Hash for UnversionedInterface {
    fn hash<H: Hasher>(&self, state: &mut H) {
        self.0.name().hash(state);
        self.0.package().name().hash(state);
    }
}

/// Denotes a package that must be supplied for resolution to continue.
#[derive(Debug)]
pub struct UnresolvedPackage {
    /// Whether any candidate packages must match the version exactly.
    exact: bool,
    /// The ID of the package.
    id: PackageIdentifier,
    /// The selected component, if any.
    selected: Option<(PackageIdentifier, Component)>,
}

impl UnresolvedPackage {
    /// Gets the requested ID of this package.
    pub fn id(&self) -> &PackageIdentifier {
        &self.id
    }

    /// Whether the resolved component must match in version exactly. Otherwise, any version
    /// that is greater only in patch (satisfying the `semver` equation `actual = ^requested`)
    /// is allowed.
    pub fn exact(&self) -> bool {
        self.exact
    }

    /// Whether a component has been provided for this package.
    pub fn is_resolved(&self) -> bool {
        self.selected.is_some()
    }

    /// Resolves this package using the given component and ID. The resolved component must match in name and
    /// have a compatible version, otherwise, a panic will occur.
    pub fn resolve(&mut self, id: PackageIdentifier, component: Component) {
        assert!(
            self.id.name() == id.name(),
            "Package names were not the same."
        );
        if self.exact {
            assert!(
                self.id.version() == id.version(),
                "Package {} versions did not match exactly: expected {:?} but got {:?}",
                self.id.name(),
                self.id.version(),
                id.version()
            );
        } else {
            assert!(
                PartialVersionRef(self.id.version()).matches(&PartialVersionRef(id.version())),
                "Package {} versions did not match: expected {:?} but got {:?}",
                self.id.name(),
                self.id.version(),
                id.version()
            );
        }

        assert!(
            replace(&mut self.selected, Some((id, component))).is_none(),
            "Package was already resolved."
        );
    }
}

/// Builds a package dependency graph which can be converted to a [`PackageContextImage`].
pub struct PackageResolver {
    /// The topological graph.
    graph: TopoSort<PackageName>,
    /// The current versions and binaries of loaded packages.
    chosen_packages: FxHashMap<PackageName, PackageResolutionInfo>,
    /// The top-level packages from which all dependencies originate.
    top_level_packages: FxHashMap<PackageName, Option<Version>>,
    /// The list of packages that the user has yet to resolve.
    unresolved_packages: Vec<UnresolvedPackage>,
    /// The set of packages that are currently undergoing resolution.
    to_resolve: Vec<PackageIdentifier>,
    /// The linker that will be used to resolve host imports, and a mapping from interface name to version.
    linker_packages: Arc<(Linker, FxHashSet<UnversionedInterface>)>,
}

impl PackageResolver {
    /// Creates a new package resolver for the requested set of top-level packages and host linker.
    /// Panics if the same package appears in the list multiple times.
    pub fn new(ids: impl IntoIterator<Item = PackageIdentifier>, linker: &Linker) -> Self {
        let mut top_level_packages = FxHashMap::default();

        for id in ids {
            assert!(
                top_level_packages
                    .insert(id.name().clone(), id.version().cloned())
                    .is_none(),
                "Duplicate top-level packages."
            );
        }

        let mut to_resolve = Vec::with_capacity(top_level_packages.len());
        for (name, version) in &top_level_packages {
            to_resolve.push(PackageIdentifier::new(name.clone(), version.clone()));
        }

        Self {
            graph: TopoSort::new(),
            chosen_packages: FxHashMap::with_capacity_and_hasher(
                top_level_packages.len(),
                FxBuildHasher::default(),
            ),
            top_level_packages,
            unresolved_packages: Vec::with_capacity(to_resolve.len()),
            to_resolve,
            linker_packages: Arc::new((linker.clone(), Self::host_package_set(linker))),
        }
    }

    /// The list of packages that must be provided for resolution to continue.
    pub fn unresolved(&mut self) -> &mut [UnresolvedPackage] {
        &mut self.unresolved_packages
    }

    /// Attempts to resolve the dependency graph into a set of distinct, versioned packages.
    /// Fails if more packages need to be provided, if packages have conflicting dependencies,
    /// or if packages have cyclic dependencies.
    pub fn resolve(mut self) -> Result<PackageContextImage, PackageResolverError> {
        self.clear_unresolved();
        while let Some(next) = self.to_resolve.pop() {
            if let Some(info) = self.chosen_packages.get_mut(next.name()) {
                if Self::upgrade(&next, &mut info.version, info.exact)? {
                    match info.state {
                        PackageResolutionState::Unresolved(idx) => {
                            self.unresolved_packages[idx as usize].id = next;
                        }
                        PackageResolutionState::Resolved(_) => {
                            let index = self.unresolved_packages.len() as u16;
                            info.state = PackageResolutionState::Unresolved(index);
                            self.unresolved_packages.push(UnresolvedPackage {
                                exact: self.top_level_packages.contains_key(next.name()),
                                id: next,
                                selected: None,
                            });
                        }
                        PackageResolutionState::InGraph(_) => {
                            self.reset_graph();
                        }
                    }
                } else if let PackageResolutionState::Resolved(x) = &mut info.state {
                    let mut mismatch_err = std::result::Result::Ok(());
                    self.graph.insert(
                        next.name().clone(),
                        x.imports().instances().filter_map(|(x, _)| {
                            if let Some(host) = self.linker_packages.1.get(UnversionedInterface::ref_cast(x)) {
                                if !PartialVersionRef(x.package().version()).matches(&PartialVersionRef(host.0.package().version())) {
                                    mismatch_err = Err(PackageResolverError::IncompatibleVersions(x.package().name().clone(), x.package().version().cloned(), host.0.package().version().cloned()));
                                }
                                None
                            }
                            else {
                                self.to_resolve.push(x.package().clone());
                                Some(x.package().name().clone())
                            }
                        }),
                    );

                    mismatch_err?;
                    info.state = PackageResolutionState::InGraph(x.clone());
                }
            } else {
                let index = self.unresolved_packages.len() as u16;
                self.chosen_packages.insert(
                    next.name().clone(),
                    PackageResolutionInfo {
                        version: next.version().cloned(),
                        exact: self.top_level_packages.contains_key(next.name()),
                        state: PackageResolutionState::Unresolved(index),
                    },
                );
                self.unresolved_packages.push(UnresolvedPackage {
                    exact: self.top_level_packages.contains_key(next.name()),
                    id: next,
                    selected: None,
                });
            }
        }

        if self.unresolved_packages.is_empty() {
            self.into_package_topology()
        } else {
            std::result::Result::Err(PackageResolverError::MissingPackages(self))
        }
    }

    /// Resets the dependency graph. Called after bumping the version of an existing
    /// dependency during resolution.
    fn reset_graph(&mut self) {
        self.graph = TopoSort::new();
        self.unresolved_packages.clear();
        self.to_resolve.clear();
        self.to_resolve.extend(
            self.top_level_packages
                .iter()
                .map(|(a, b)| PackageIdentifier::new(a.clone(), b.clone())),
        );

        for pkg in self.chosen_packages.values_mut() {
            if let PackageResolutionState::InGraph(x) = &mut pkg.state {
                pkg.state = PackageResolutionState::Resolved(x.clone());
            }
        }
    }

    /// Extracts the topologically-sorted list of dependencies from this resolver.
    fn into_package_topology(mut self) -> Result<PackageContextImage, PackageResolverError> {
        let mut res = PackageContextImageInner {
            packages: Vec::with_capacity(self.graph.len()),
            transitive_dependencies: PackageFlagsList::new(false, self.graph.len()),
            transitive_dependents: PackageFlagsList::new(false, self.graph.len()),
            top_level_packages: BitVec::repeat(false, self.graph.len()),
            package_map: FxHashMap::with_capacity_and_hasher(
                self.graph.len(),
                FxBuildHasher::default(),
            ),
            linker_packages: self.linker_packages.clone()
        };

        self.load_packages_and_dependencies(&mut res)?;
        self.compute_inverse_dependencies(&mut res);

        std::result::Result::Ok(PackageContextImage(Arc::new(res)))
    }

    /// Loads a package's component and its transitive dependency list into the output image.
    fn load_packages_and_dependencies(
        &mut self,
        res: &mut PackageContextImageInner,
    ) -> Result<(), PackageResolverError> {
        for x in self.graph.nodes() {
            let name = x.map_err(|_| PackageResolverError::CyclicPackageDependency())?;
            let chosen = &self.chosen_packages[name];

            if let PackageResolutionState::InGraph(x) = &chosen.state {
                let idx = res.packages.len();

                if chosen.exact {
                    res.top_level_packages.set(idx, true);
                }

                res.package_map.insert(name.clone(), idx as u16);
                res.packages.push(ResolvedPackage {
                    id: PackageIdentifier::new(name.clone(), chosen.version.clone()),
                    component: x.clone(),
                });

                let mut edit = res.transitive_dependencies.edit(idx);
                edit.set(idx, true);
                for dependency in &self.graph[name] {
                    edit.or_with(res.package_map[dependency] as usize);
                }
            } else {
                unreachable!();
            }
        }

        std::result::Result::Ok(())
    }

    /// Computes the transitive dependents for each package in the image.
    fn compute_inverse_dependencies(&self, res: &mut PackageContextImageInner) {
        for i in (0..res.packages.len()).rev() {
            let mut edit = res.transitive_dependents.edit(i);
            edit.set(i, true);
            let name = res.packages[i].id.name();
            for i in &self.graph[name] {
                edit.or_into(res.package_map[i] as usize);
            }
        }
    }

    /// Removes any newly-resolved packages from the list of requested external packages.
    fn clear_unresolved(&mut self) {
        let mut i = 0;
        self.unresolved_packages.retain(|resolved| {
            if let Some((id, pkg)) = &resolved.selected {
                let info = self
                    .chosen_packages
                    .get_mut(id.name())
                    .expect("Package was not in map.");
                info.version = id.version().cloned();
                info.state = PackageResolutionState::Resolved(pkg.clone());
                self.to_resolve.push(id.clone());
                false
            } else {
                self.chosen_packages
                    .get_mut(resolved.id.name())
                    .expect("Package was not in map.")
                    .state = PackageResolutionState::Unresolved(i);
                i += 1;
                true
            }
        });
    }

    /// Takes the maximum between two dependency versions, returning whether the version
    /// changed in the process.
    fn upgrade(
        id: &PackageIdentifier,
        mut current: &mut Option<Version>,
        exact: bool,
    ) -> Result<bool, PackageResolverError> {
        if exact {
            if id.version() == current.as_ref() {
                std::result::Result::Ok(false)
            } else {
                std::result::Result::Err(PackageResolverError::IncompatibleVersions(
                    id.name().clone(),
                    current.clone(),
                    id.version().cloned(),
                ))
            }
        } else {
            match (&mut current, id.version()) {
                (None, None) => std::result::Result::Ok(false),
                (None, Some(x)) => {
                    *current = Some(x.clone());
                    std::result::Result::Ok(true)
                }
                (Some(_), None) => std::result::Result::Ok(false),
                (Some(a), Some(b)) => {
                    if a.major == b.major && a.minor == b.minor {
                        match a.patch.cmp(&b.patch) {
                            std::cmp::Ordering::Less => {
                                a.patch = b.patch;
                                std::result::Result::Ok(true)
                            }
                            std::cmp::Ordering::Equal => match a.pre.cmp(&b.pre) {
                                std::cmp::Ordering::Less => {
                                    a.pre = b.pre.clone();
                                    std::result::Result::Ok(true)
                                }
                                std::cmp::Ordering::Equal => {
                                    if a.build < b.build {
                                        a.build = b.build.clone();
                                        std::result::Result::Ok(true)
                                    } else {
                                        std::result::Result::Ok(false)
                                    }
                                }
                                std::cmp::Ordering::Greater => std::result::Result::Ok(false),
                            },
                            std::cmp::Ordering::Greater => std::result::Result::Ok(false),
                        }
                    } else {
                        std::result::Result::Err(PackageResolverError::IncompatibleVersions(
                            id.name().clone(),
                            Some(a.clone()),
                            Some(b.clone()),
                        ))
                    }
                }
            }
        }
    }

    /// Collects the set of host packages from the input linker.
    fn host_package_set(linker: &Linker) -> FxHashSet<UnversionedInterface> {
        let mut result = FxHashSet::<UnversionedInterface>::with_capacity_and_hasher(linker.instances().len(), Default::default());
        for (id, _) in linker.instances() {
            assert!(result.insert(UnversionedInterface(id.clone())), "Multiple versions of the same host interface declared.");
        }
        result
    }
}

impl std::fmt::Debug for PackageResolver {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PackageResolver").finish()
    }
}

/// Copies one linker instance to another.
fn copy_instance(old: &LinkerInstance, new: &mut LinkerInstance) {
    for (name, func) in old.funcs() {
        new.define_func(name, func).expect("Could not copy function between instances.")
    }
    
    for (name, resource) in old.resources() {
        new.define_resource(name, resource).expect("Could not copy resource between instances.")
    }
}

/// Describes an issue with package dependency resolution.
#[derive(Debug)]
pub enum PackageResolverError {
    /// Two or more packages were mutually dependent on each other.
    CyclicPackageDependency(),
    /// The same package was required multiple times with conflicting
    /// `semver` versions.
    IncompatibleVersions(PackageName, Option<Version>, Option<Version>),
    /// The resolver needs additional packages to be supplied externally.
    MissingPackages(PackageResolver),
}

/// Describes a target state which can be applied to a [`PackageContext`]. Each
/// `PackageContextImage` is an instantaneous "snapshot" of what a context should look
/// like after loading and unloading the requisite packages.
#[derive(Clone, Debug, Default)]
pub struct PackageContextImage(Arc<PackageContextImageInner>);

impl PackageContextImage {
    /// Creates a transition for the given context based upon this image.
    fn as_transition<'a>(&'a self, ctx: &'a PackageContext) -> PackageContextTransitionBuilder<'a> {
        let mut transition = PackageContextTransitionBuilder {
            state: ctx.state,
            context: ctx,
            image: self,
            to_load: Vec::with_capacity(self.0.packages.len()),
            to_unload: Vec::with_capacity(ctx.packages.len()),
        };

        let mut old_touched_dead = BitVec::<usize, Lsb0>::repeat(false, 2 * ctx.packages.len());
        let (old_touched, old_dead) = old_touched_dead.split_at_mut(ctx.packages.len());

        for i in 0..self.0.packages.len() {
            let pkg = &self.0.packages[i];
            if let Some(old_i) = ctx.image.0.package_map.get(pkg.id.name()) {
                if pkg.id != ctx.image.0.packages[*old_i as usize].id {
                    old_dead[..] |= ctx.image.0.transitive_dependents.get(*old_i as usize);
                }

                if old_dead[*old_i as usize] {
                    transition
                        .to_unload
                        .push(ctx.image.0.packages[*old_i as usize].id.clone());
                    transition.to_load.push(PackageBuildOptions {
                        resolved: pkg,
                        linker: Cow::Borrowed(&self.0.linker_packages.0),
                    });
                } else {
                    old_touched.set(*old_i as usize, true);
                }
            } else {
                transition.to_load.push(PackageBuildOptions {
                    resolved: pkg,
                    linker: Cow::Borrowed(&self.0.linker_packages.0),
                });
            }
        }

        for i in old_touched.iter_zeros() {
            transition
                .to_unload
                .push(ctx.image.0.packages[i].id.clone());
        }

        transition.to_unload.reverse();

        transition
    }

    /// Obtains an iterator over all packages that this one depends upon, directly or indirectly.
    pub fn transitive_dependencies<'a>(
        &'a self,
        id: &PackageIdentifier,
    ) -> impl 'a + Iterator<Item = &'a PackageIdentifier> {
        self.0
            .package_map
            .get(id.name())
            .and_then(|x| (&self.0.packages[*x as usize].id == id).then_some(x))
            .into_iter()
            .flat_map(|x| self.0.transitive_dependencies.get(*x as usize).iter_ones())
            .map(|x| &self.0.packages[x].id)
    }

    /// Obtains an iterator over all packages that depend upon this one, directly or indirectly.
    pub fn transitive_dependents<'a>(
        &'a self,
        id: &PackageIdentifier,
    ) -> impl 'a + Iterator<Item = &'a PackageIdentifier> {
        self.0
            .package_map
            .get(id.name())
            .and_then(|x| (&self.0.packages[*x as usize].id == id).then_some(x))
            .into_iter()
            .flat_map(|x| self.0.transitive_dependents.get(*x as usize).iter_ones())
            .map(|x| &self.0.packages[x].id)
    }

    /// Obtains an iterator over all top-level packages that depend upon this one, directly or indirectly.
    pub fn top_level_dependents<'a>(
        &'a self,
        id: &PackageIdentifier,
    ) -> impl 'a + Iterator<Item = &'a PackageIdentifier> {
        let mut tmp = self.0.top_level_packages.clone();

        if let Some(pkg) = self.0.package_map.get(id.name()) {
            if &self.0.packages[*pkg as usize].id == id {
                tmp[..] &= self.0.transitive_dependents.get(*pkg as usize);
            } else {
                tmp.fill(false);
            }
        } else {
            tmp.fill(false);
        }

        tmp.iter_ones()
            .collect::<Vec<_>>()
            .into_iter()
            .map(|x| &self.0.packages[x].id)
    }
}

/// Stores the inner state for a package context.
#[derive(Debug, Default)]
struct PackageContextImageInner {
    /// An indexed list of resolved packages.
    pub packages: Vec<ResolvedPackage>,
    /// The indirect dependencies for each package.
    pub transitive_dependencies: PackageFlagsList,
    /// The indirect dependents for each package.
    pub transitive_dependents: PackageFlagsList,
    /// The subset of packages that are top-level.
    pub top_level_packages: BitVec,
    /// A mapping from package name to index in the resolved list.
    pub package_map: FxHashMap<PackageName, u16>,
    /// The linker that will be used to resolve host imports, and a mapping from interface name to version.
    linker_packages: Arc<(Linker, FxHashSet<UnversionedInterface>)>,
}

/// Facilitates the creation of new [`PackageContextTransition`]s, which switch the [`PackageContextImage`]
/// upon which a context is based.
#[derive(Debug)]
pub struct PackageContextTransitionBuilder<'a> {
    /// The original state of the context.
    state: u64,
    /// The context against which to build.
    context: &'a PackageContext,
    /// The new image.
    image: &'a PackageContextImage,
    /// The list of packages to load, along with load options.
    to_load: Vec<PackageBuildOptions<'a>>,
    /// The list of packages to unload.
    to_unload: Vec<PackageIdentifier>,
}

impl<'a> PackageContextTransitionBuilder<'a> {
    /// Creates a builder for switching the given context's state to a new image.
    pub fn new(image: &'a PackageContextImage, ctx: &'a PackageContext) -> Self {
        image.as_transition(ctx)
    }

    /// An immutable reference to the ordered list of packages that must be instantiated and loaded,
    /// along with options on how to create instances of them.
    pub fn to_load(&self) -> &[PackageBuildOptions<'a>] {
        &self.to_load
    }

    /// A mutable reference to the ordered list of packages that must be instantiated and loaded,
    /// along with options on how to create instances of them.
    pub fn to_load_mut(&mut self) -> &mut [PackageBuildOptions<'a>] {
        &mut self.to_load
    }

    /// The ordered list of packages that must be unloaded.
    pub fn to_unload(&self) -> &[PackageIdentifier] {
        &self.to_unload
    }

    /// Instantiates and links any new components to produce a final [`PackageContextTransition`].
    pub fn build(self, store: impl AsContextMut) -> Result<PackageContextTransition> {
        self.context.create_next_state(store, self)
    }
}

/// Describes how to move a context between two [`PackageContextImage`]s.
#[derive(Debug)]
pub struct PackageContextTransition {
    /// The original state of the context.
    state: u64,
    /// The image to use.
    image: PackageContextImage,
    /// The list of packages to load.
    to_load: Vec<PackageIdentifier>,
    /// The list of packages to unload.
    to_unload: Vec<PackageIdentifier>,
    /// The new set of packages for the context.
    packages: FxHashMap<PackageName, ResolvedInstance>,
}

impl PackageContextTransition {
    /// Applies this transition to the given context, updating the set of loaded packages.
    pub fn apply<T, E: wasm_runtime_layer::backend::WasmEngine>(
        self,
        store: &mut Store<T, E>,
        ctx: &mut PackageContext,
    ) -> Vec<Error> {
        ctx.apply(store, self)
    }

    /// The image upon which this transition is based.
    pub fn image(&self) -> &PackageContextImage {
        &self.image
    }

    /// The ordered list of packages that this transition will load.
    pub fn to_load(&self) -> &[PackageIdentifier] {
        &self.to_load
    }

    /// The ordered list of packages that this transition will unload.
    pub fn to_unload(&self) -> &[PackageIdentifier] {
        &self.to_unload
    }
}

/// Determines how a package's component should be instantiated and linked.
#[derive(Debug)]
pub struct PackageBuildOptions<'a> {
    /// The resolved package.
    resolved: &'a ResolvedPackage,
    /// The linker to use.
    linker: Cow<'a, Linker>,
}

impl<'a> PackageBuildOptions<'a> {
    /// The associated package ID.
    pub fn id(&self) -> &PackageIdentifier {
        &self.resolved.id
    }

    /// The component that will be instantiated.
    pub fn component(&self) -> &Component {
        &self.resolved.component
    }

    /// Gets an immutable reference to the linker which will resolve host imports.
    pub fn linker(&self) -> &Linker {
        &self.linker
    }

    /// Gets a mutable reference to the linker which will resolve host imports.
    pub fn linker_mut(&mut self) -> &mut Linker {
        self.linker.to_mut()
    }
}

/// Represents a densely-packed list of lists of bitflags used to store
/// per-package information about other packages.
#[derive(Clone, Debug, Default)]
struct PackageFlagsList {
    /// The underlying data buffer.
    data: BitVec,
    /// The amount of bits per package.
    stride: usize,
}

impl PackageFlagsList {
    /// Creates a new list of package flags, initialized to the given value.
    #[inline(always)]
    pub fn new(bit: bool, size: usize) -> Self {
        Self {
            data: BitVec::repeat(bit, size * size),
            stride: size,
        }
    }

    /// Initializes an edit of the flags for the given package, allowing for the
    /// simultaneous immutable use of flags with smaller indices during the edit.
    #[inline(always)]
    pub fn edit(&mut self, index: usize) -> PackageFlagsListEdit {
        let (rest, first) = self.data.split_at_mut(index * self.stride);

        PackageFlagsListEdit {
            editable: &mut first[..self.stride],
            rest,
            stride: self.stride,
        }
    }

    /// Gets a slice associated with the given package at the provided index.
    #[inline(always)]
    pub fn get(&self, index: usize) -> &BitSlice {
        let base = index * self.stride;
        &self.data[base..base + self.stride]
    }
}

impl BitOrAssign<&PackageFlagsList> for PackageFlagsList {
    #[inline(always)]
    fn bitor_assign(&mut self, rhs: &PackageFlagsList) {
        self.data |= &rhs.data;
    }
}

/// Represents an ongoing edit operation to a package flags list. This allows
/// for editing one part of the list while referencing other parts.
struct PackageFlagsListEdit<'a> {
    /// The part of the bit vector that is being edited.
    editable: &'a mut BitSlice<BitSafeUsize>,
    /// Everything prior to the editable part which may be immutably referenced.
    rest: &'a mut BitSlice<BitSafeUsize>,
    /// The number of bits per package.
    stride: usize,
}

impl<'a> PackageFlagsListEdit<'a> {
    /// Computes the bitwise or into the editable region with the package at the given index.
    #[inline(always)]
    pub fn or_with(&mut self, index: usize) {
        let base = index * self.stride;
        *self.editable |= &self.rest[base..base + self.stride];
    }

    /// Computes the bitwise or of the editable region with the package into the given index.
    #[inline(always)]
    pub fn or_into(&mut self, index: usize) {
        let base = index * self.stride;
        self.rest[base..base + self.stride] |= &*self.editable;
    }
}

impl<'a> Deref for PackageFlagsListEdit<'a> {
    type Target = BitSlice<BitSafeUsize>;

    #[inline(always)]
    fn deref(&self) -> &Self::Target {
        self.editable
    }
}

impl<'a> DerefMut for PackageFlagsListEdit<'a> {
    #[inline(always)]
    fn deref_mut(&mut self) -> &mut Self::Target {
        self.editable
    }
}

/// A package that consists of an identifier and an instantiated component.
#[derive(Clone, Debug)]
struct ResolvedInstance {
    /// The version of the instance.
    version: Option<Version>,
    /// The instantiated component.
    instance: Instance,
}

/// Represents an instantiated WebAssembly component that is owned by a [`PackageContext`].
#[derive(Debug)]
pub struct LoadedPackage<'a> {
    /// The package's ID.
    name: PackageIdentifier,
    /// The instance associated with the package.
    instance: &'a Instance,
}

impl<'a> LoadedPackage<'a> {
    /// The identifier of the package.
    pub fn id(&self) -> &PackageIdentifier {
        &self.name
    }

    /// The exports of the package.
    pub fn exports(&self) -> &Exports {
        self.instance.exports()
    }
}

/// Manages a set of loaded WebAssembly components. `PackageContext`s are responsible for loading, linking,
/// and unloading packages to match the state described by a [`PackageContextImage`]. Contexts also
/// provide access to the exports of loaded packages.
pub struct PackageContext {
    /// The state of the context.
    state: u64,
    /// The context's image.
    image: PackageContextImage,
    /// The set of currently-loaded packages.
    packages: FxHashMap<PackageName, ResolvedInstance>,
}

impl PackageContext {
    /// Creates a new package context with nothing loaded.
    pub fn new() -> Self {
        Self {
            state: Self::next_state(),
            image: PackageContextImage::default(),
            packages: FxHashMap::default(),
        }
    }

    /// The image upon which this context is based.
    pub fn image(&self) -> &PackageContextImage {
        &self.image
    }

    /// Gets the package with the provided name, if any.
    pub fn package(&self, name: &PackageName) -> Option<LoadedPackage> {
        self.packages.get(name).map(|x| LoadedPackage {
            name: PackageIdentifier::new(name.clone(), x.version.clone()),
            instance: &x.instance,
        })
    }

    /// Gets an iterator over all loaded packages.
    pub fn packages(&self) -> impl '_ + Iterator<Item = LoadedPackage> {
        self.packages.iter().map(|(name, x)| LoadedPackage {
            name: PackageIdentifier::new(name.clone(), x.version.clone()),
            instance: &x.instance,
        })
    }

    /// Applies a state transition to this context.
    fn apply<T, E: wasm_runtime_layer::backend::WasmEngine>(
        &mut self,
        ctx: &mut Store<T, E>,
        transition: PackageContextTransition,
    ) -> Vec<Error> {
        assert!(
            self.state == transition.state,
            "Transition was not created for the current context's state."
        );
        let mut errors = Vec::new();

        for to_unload in &transition.to_unload {
            let inst = self
                .packages
                .remove(to_unload.name())
                .expect("Could not find instance.");
            errors.extend(inst.instance.drop(ctx).expect("Could not drop instance."));
        }

        self.state = Self::next_state();
        self.packages = transition.packages;
        self.image = transition.image;

        errors
    }

    /// Creates a context transition from a builder.
    fn create_next_state(
        &self,
        mut ctx: impl AsContextMut,
        mut transition: PackageContextTransitionBuilder,
    ) -> Result<PackageContextTransition> {
        assert!(
            self.state == transition.state,
            "Transition was not created for the current context's state."
        );
        let mut next_packages = self.packages.clone();

        for to_unload in &transition.to_unload {
            assert!(
                next_packages.remove(to_unload.name()).is_some(),
                "Did not find package to remove"
            );
        }

        let mut to_load = Vec::with_capacity(transition.to_load.len());

        for step in &mut transition.to_load {
            to_load.push(step.resolved.id.clone());
            Self::load_new(&mut next_packages, ctx.as_context_mut(), step, &transition.image.0.linker_packages.1)?;
        }

        Ok(PackageContextTransition {
            state: self.state,
            image: transition.image.clone(),
            to_load,
            to_unload: transition.to_unload,
            packages: next_packages,
        })
    }

    /// Instantiates a new package, linking all of its imports in the process.
    fn load_new(
        packages: &mut FxHashMap<PackageName, ResolvedInstance>,
        ctx: impl AsContextMut,
        step: &mut PackageBuildOptions,
        linker_versions: &FxHashSet<UnversionedInterface>
    ) -> Result<()> {
        let linker = step.linker.to_mut();
        let mut tmp_linker = Linker::default();

        for (interface, _) in step.resolved.component.imports().instances() {
            if let Some(host) = linker_versions.get(UnversionedInterface::ref_cast(interface)) {
                if host.0.package().version() != interface.package().version() {
                    let tmp_instance = tmp_linker.define_instance(interface.clone()).expect("Could not define temporary instance.");
                    copy_instance(linker.instance(&host.0).context("Could not find host interface.")?, tmp_instance);
                    copy_instance(tmp_instance, linker.define_instance(interface.clone()).context("Conflicting host interface versions.")?);
                }
            }
            else {
                let exporting_package = &packages[interface.package().name()];
                let name = InterfaceIdentifier::new(
                    PackageIdentifier::new(
                        interface.package().name().clone(),
                        exporting_package.version.clone(),
                    ),
                    interface.name(),
                );
                if let Some(exported) = exporting_package.instance.exports().instance(&name) {
                    Self::fill_linker(linker, interface, exported)?;
                } else {
                    bail!(
                        "Package {} required missing interface {interface}",
                        step.resolved.id
                    );
                }
            }
        }

        assert!(
            packages
                .insert(
                    step.resolved.id.name().clone(),
                    ResolvedInstance {
                        version: step.resolved.id.version().cloned(),
                        instance: linker.instantiate(ctx, &step.resolved.component)?
                    }
                )
                .is_none(),
            "Added duplicate packages."
        );
        Ok(())
    }

    /// Adds all exports from the provided interface instantiation to the linker, failing if any
    /// externals are already defined.
    fn fill_linker(
        linker: &mut Linker,
        id: &InterfaceIdentifier,
        instance: &ExportInstance,
    ) -> Result<()> {
        let to_fill = linker.define_instance(id.clone())?;

        for (name, func) in instance.funcs() {
            to_fill.define_func(name, func)?;
        }

        for (name, resource_ty) in instance.resources() {
            to_fill.define_resource(name, resource_ty)?;
        }

        Ok(())
    }

    /// Gets the next internal state which tracks that image that a context currently has.
    fn next_state() -> u64 {
        /// A counter which ensures that each context state is unique.
        static ID_COUNTER: AtomicU64 = AtomicU64::new(0);
        ID_COUNTER.fetch_add(1, Ordering::Relaxed)
    }
}

impl Default for PackageContext {
    fn default() -> Self {
        Self::new()
    }
}

impl std::fmt::Debug for PackageContext {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PackageContext")
            .field(
                "packages",
                &self
                    .packages
                    .iter()
                    .map(|(k, v)| PackageIdentifier::new(k.clone(), v.version.clone()))
                    .collect::<Vec<_>>(),
            )
            .finish()
    }
}
