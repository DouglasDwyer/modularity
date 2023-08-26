#![allow(warnings)]
#![forbid(unsafe_code)]

use anyhow::*;
use anyhow::Error;
use bitvec::access::*;
use bitvec::prelude::*;
use fxhash::*;
use semver::*;
use std::borrow::*;
use std::mem::*;
use std::ops::*;
use std::sync::*;
use std::sync::atomic::*;
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
            (Some(a), Some(b)) => a.major == b.major && a.minor == b.minor && a.patch <= b.patch
        }
    }
}

impl<'a> PartialOrd for PartialVersionRef<'a> {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(match (&self.0, &other.0) {
            (None, None) => std::cmp::Ordering::Equal,
            (Some(_), None) => std::cmp::Ordering::Greater,
            (None, Some(_)) => std::cmp::Ordering::Less,
            (Some(a), Some(b)) => a.cmp(b)
        })
    }
}

impl<'a> Ord for PartialVersionRef<'a> {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        match (&self.0, &other.0) {
            (None, None) => std::cmp::Ordering::Equal,
            (Some(_), None) => std::cmp::Ordering::Greater,
            (None, Some(_)) => std::cmp::Ordering::Less,
            (Some(a), Some(b)) => a.cmp(b)
        }
    }
}

#[derive(Clone, Debug)]
struct ResolvedPackage {
    pub id: PackageIdentifier,
    pub component: Component
}

/// Stores transient information about a package during version resolution.
#[derive(Clone, Debug)]
struct PackageResolutionInfo {
    /// The maximum version of the package loaded so far.
    pub version: Option<Version>,
    /// Whether any candidate packages must match in version exactly.
    pub exact: bool,
    /// Whether this package is resolved and in the component graph.
    pub state: PackageResolutionState
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

/// Denotes a package that must be supplied for resolution to continue.
#[derive(Clone, Debug)]
pub struct UnresolvedPackage {
    /// Whether any candidate packages must match the version exactly.
    exact: bool,
    /// The ID of the package.
    id: PackageIdentifier,
    /// The selected component, if any.
    selected: Option<(PackageIdentifier, Component)>
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
        assert!(self.id.name() == id.name(), "Package names were not the same.");
        if self.exact {
            assert!(self.id.version() == id.version(), "Package {} versions did not match exactly: expected {:?} but got {:?}", self.id.name(), self.id.version(), id.version());
        }
        else {
            assert!(PartialVersionRef(self.id.version()).matches(&PartialVersionRef(id.version())), "Package {} versions did not match: expected {:?} but got {:?}", self.id.name(), self.id.version(), id.version());
        }

        assert!(replace(&mut self.selected, Some((id, component))).is_none(), "Package was already resolved.");
    }
}

/// Builds a package dependency graph which can be converted to a [`PackageContextImage`].
#[derive(Clone)]
pub struct PackageResolver {
    /// The topological graph.
    graph: TopoSort<PackageName>,
    /// The current versions and binaries of loaded packages.
    chosen_packages: FxHashMap<PackageName, PackageResolutionInfo>,
    /// The top-level packages from which all dependencies originate.
    top_level_packages: FxHashMap<PackageName, Option<Version>>,
    /// The list of packages that the user has yet to resolve.
    unresolved_packages: Vec<UnresolvedPackage>,
    to_resolve: Vec<PackageIdentifier>,
    linker: Linker
}

impl PackageResolver {
    pub fn new(ids: impl IntoIterator<Item = PackageIdentifier>, linker: Linker) -> Self {
        let mut top_level_packages = FxHashMap::default();

        for id in ids {
            assert!(top_level_packages.insert(id.name().clone(), id.version().cloned()).is_none(), "Duplicate top-level packages.");            
        }

        let mut to_resolve = Vec::with_capacity(top_level_packages.len());
        for (name, version) in &top_level_packages {
            to_resolve.push(PackageIdentifier::new(name.clone(), version.clone()));
        }

        Self {
            graph: TopoSort::new(),
            chosen_packages: FxHashMap::with_capacity_and_hasher(top_level_packages.len(), FxBuildHasher::default()),
            top_level_packages,
            unresolved_packages: Vec::with_capacity(to_resolve.len()),
            to_resolve,
            linker
        }
    }

    pub fn unresolved(&mut self) -> &mut [UnresolvedPackage] {
        &mut self.unresolved_packages
    }

    pub fn resolve(mut self) -> Result<PackageContextImage, PackageResolverError> {
        self.clear_unresolved();
        while let Some(next) = self.to_resolve.pop() {
            if let Some(info) = self.chosen_packages.get_mut(next.name()) {
                if Self::upgrade(&next, &mut info.version, info.exact)? {
                    match info.state {
                        PackageResolutionState::Unresolved(idx) => {
                            self.unresolved_packages[idx as usize].id = next;
                        },
                        PackageResolutionState::Resolved(_) => {
                            let index = self.unresolved_packages.len() as u16;
                            info.state = PackageResolutionState::Unresolved(index);
                            self.unresolved_packages.push(UnresolvedPackage { exact: self.top_level_packages.contains_key(next.name()), id: next, selected: None });
                        },
                        PackageResolutionState::InGraph(_) => {
                            self.reset_graph();
                        },
                    }
                }
                else if let PackageResolutionState::Resolved(x) = &mut info.state {
                    self.graph.insert(next.name().clone(), x.imports().instances().filter_map(|(x, _)| self.linker.instance(x).is_none().then(|| {
                        self.to_resolve.push(x.package().clone());
                        x.package().name().clone()
                    })));

                    info.state = PackageResolutionState::InGraph(x.clone());
                }                
            }
            else {
                let index = self.unresolved_packages.len() as u16;
                self.chosen_packages.insert(next.name().clone(), PackageResolutionInfo { version: next.version().cloned(), exact: self.top_level_packages.contains_key(next.name()), state: PackageResolutionState::Unresolved(index) });
                self.unresolved_packages.push(UnresolvedPackage { exact: self.top_level_packages.contains_key(next.name()), id: next, selected: None });
            }
        }

        if self.unresolved_packages.is_empty() {
            self.into_package_topology()
        }
        else {
            std::result::Result::Err(PackageResolverError::MissingPackages(self))
        }
    }

    fn reset_graph(&mut self) {
        self.graph = TopoSort::new();
        self.unresolved_packages.clear();
        self.to_resolve.clear();
        self.to_resolve.extend(self.top_level_packages.iter().map(|(a, b)| PackageIdentifier::new(a.clone(), b.clone())));
    
        for pkg in self.chosen_packages.values_mut() {
            if let PackageResolutionState::InGraph(x) = &mut pkg.state {
                pkg.state = PackageResolutionState::Resolved(x.clone());
            }
        }
    }

    fn into_package_topology(mut self) -> Result<PackageContextImage, PackageResolverError> {
        let mut res = PackageContextImageInner {
            packages: Vec::with_capacity(self.graph.len()),
            transitive_dependencies: PackageFlagsList::new(false, self.graph.len()),
            transitive_dependents: PackageFlagsList::new(false, self.graph.len()),
            package_map: FxHashMap::with_capacity_and_hasher(self.graph.len(), FxBuildHasher::default()),
            linker: Linker::default()
        };

        self.load_packages_and_dependencies(&mut res)?;
        self.compute_inverse_dependencies(&mut res);
        res.linker = self.linker;

        std::result::Result::Ok(PackageContextImage(Arc::new(res)))
    }

    fn load_packages_and_dependencies(&mut self, res: &mut PackageContextImageInner) -> Result<(), PackageResolverError> {
        for x in self.graph.nodes() {
            let name = x.map_err(|_| PackageResolverError::CyclicPackageDependency())?;
            let chosen = &self.chosen_packages[name];
            if let PackageResolutionState::InGraph(x) = &chosen.state {
                let idx = res.packages.len();
                res.package_map.insert(name.clone(), idx as u16);
                res.packages.push(ResolvedPackage { id: PackageIdentifier::new(name.clone(), chosen.version.clone()), component: x.clone() });
        
                let mut edit = res.transitive_dependencies.edit(idx);
                edit.set(idx, true);
                for dependency in &self.graph[name] {
                    edit.or_with(res.package_map[dependency] as usize);
                }
            }
            else {
                unreachable!();
            }
        }

        std::result::Result::Ok(())
    }

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

    fn clear_unresolved(&mut self) {
        let mut i = 0;
        self.unresolved_packages.retain(|resolved| {
            if let Some((id, pkg)) = &resolved.selected {
                let info = self.chosen_packages.get_mut(id.name()).expect("Package was not in map.");
                info.version = id.version().cloned();
                info.state = PackageResolutionState::Resolved(pkg.clone());
                self.to_resolve.push(id.clone());
                false
            }
            else {
                self.chosen_packages.get_mut(resolved.id.name()).expect("Package was not in map.").state = PackageResolutionState::Unresolved(i);
                i += 1;
                true
            }
        });
    }

    fn upgrade(id: &PackageIdentifier, mut current: &mut Option<Version>, exact: bool) -> Result<bool, PackageResolverError> {
        if exact {
            if id.version() == current.as_ref() {
                std::result::Result::Ok(false)
            }
            else {
                std::result::Result::Err(PackageResolverError::IncompatibleVersions(id.name().clone(), current.clone(), id.version().cloned()))
            }
        }
        else {
            match (&mut current, id.version()) {
                (None, None) => std::result::Result::Ok(false),
                (None, Some(x)) => { *current = Some(x.clone()); std::result::Result::Ok(true) },
                (Some(_), None) => std::result::Result::Ok(false),
                (Some(a), Some(b)) => if a.major == b.major && a.minor == b.minor {
                    if a.patch < b.patch {
                        a.patch = b.patch;
                        std::result::Result::Ok(true)
                    }
                    else if a.patch == b.patch && a.pre != b.pre {
                        std::result::Result::Err(PackageResolverError::IncompatibleVersions(id.name().clone(), Some(a.clone()), Some(b.clone())))
                    }
                    else {
                        std::result::Result::Ok(false)
                    }
                } else { std::result::Result::Err(PackageResolverError::IncompatibleVersions(id.name().clone(), Some(a.clone()), Some(b.clone()))) },
            }
        }
    }
}

impl std::fmt::Debug for PackageResolver {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PackageResolver").finish()
    }
}

#[derive(Clone, Debug)]
pub enum PackageResolverError {
    CyclicPackageDependency(),
    IncompatibleVersions(PackageName, Option<Version>, Option<Version>),
    MissingPackages(PackageResolver)
}

#[derive(Clone, Debug, Default)]
pub struct PackageContextImage(Arc<PackageContextImageInner>);

impl PackageContextImage {
    pub fn as_transition(&self, ctx: &PackageContext) -> PackageContextTransition {
        let mut transition = PackageContextTransition {
            state: ctx.state,
            image: self,
            steps: Vec::with_capacity(self.0.packages.len() + ctx.packages.len()),
            num_removed: 0
        };

        let mut add_steps = Vec::with_capacity(self.0.packages.len());

        let mut old_touched_dead = BitVec::<usize, Lsb0>::repeat(false, 2 * ctx.packages.len());
        let (old_touched, old_dead) = old_touched_dead.split_at_mut(ctx.packages.len());

        for i in 0..self.0.packages.len() {
            let pkg = &self.0.packages[i];
            if let Some(old_i) = ctx.image.0.package_map.get(pkg.id.name()) {
                if pkg.id != ctx.image.0.packages[*old_i as usize].id {
                    old_dead[..] |= ctx.image.0.transitive_dependents.get(*old_i as usize);
                }

                if old_dead[*old_i as usize] {
                    transition.steps.push(PackageContextTransitionStep::UnloadPackage(UnloadPackage { id: Cow::Owned(ctx.image.0.packages[*old_i as usize].id.clone()) }));
                    add_steps.push(PackageContextTransitionStep::LoadPackage(LoadPackage { package: pkg, linker: Cow::Borrowed(&self.0.linker) }));
                }
                else {
                    old_touched.set(*old_i as usize, true);
                }
            }
            else {
                add_steps.push(PackageContextTransitionStep::LoadPackage(LoadPackage { package: pkg, linker: Cow::Borrowed(&self.0.linker) }));
            }
        }

        for i in old_touched.iter_zeros() {
            transition.steps.push(PackageContextTransitionStep::UnloadPackage(UnloadPackage { id: Cow::Owned(ctx.image.0.packages[i].id.clone()) }))
        }

        transition.num_removed = transition.steps.len();
        transition.steps.reverse();
        transition.steps.extend(add_steps);

        transition
    }
}

#[derive(Debug, Default)]
struct PackageContextImageInner {
    pub packages: Vec<ResolvedPackage>,
    pub transitive_dependencies: PackageFlagsList,
    pub transitive_dependents: PackageFlagsList,
    pub package_map: FxHashMap<PackageName, u16>,
    pub linker: Linker
}



#[derive(Debug)]
pub struct PackageContextTransition<'a> {
    state: u64,
    image: &'a PackageContextImage,
    steps: Vec<PackageContextTransitionStep<'a>>,
    num_removed: usize
}

impl<'a> PackageContextTransition<'a> {
    pub fn image(&self) -> &PackageContextImage {
        self.image
    }

    pub fn steps(&self) -> &[PackageContextTransitionStep<'a>] {
        &self.steps
    }

    pub fn steps_mut(&mut self) -> &mut [PackageContextTransitionStep<'a>] {
        &mut self.steps
    }
}

#[derive(Debug)]
pub enum PackageContextTransitionStep<'a> {
    LoadPackage(LoadPackage<'a>),
    UnloadPackage(UnloadPackage<'a>)
}

#[derive(Debug)]
pub struct LoadPackage<'a> {
    package: &'a ResolvedPackage,
    linker: Cow<'a, Linker>
}

impl<'a> LoadPackage<'a> {
    pub fn id(&self) -> &PackageIdentifier {
        &self.package.id
    }

    pub fn linker(&self) -> &Linker {
        &self.linker
    }

    pub fn linker_mut(&mut self) -> &mut Linker {
        self.linker.to_mut()
    }
}

#[derive(Debug)]
pub struct UnloadPackage<'a> {
    id: Cow<'a, PackageIdentifier>
}

impl<'a> UnloadPackage<'a> {
    pub fn id(&self) -> &PackageIdentifier {
        &self.id
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

#[derive(Debug)]
struct ResolvedInstance {
    version: Option<Version>,
    instance: Instance
}

pub struct PackageContext {
    state: u64,
    image: PackageContextImage,
    packages: FxHashMap<PackageName, ResolvedInstance>
}

impl PackageContext {
    pub fn new() -> Self {
        Self { state: Self::next_state(), image: PackageContextImage::default(), packages: FxHashMap::default() }
    }

    pub fn apply<T, E: wasm_runtime_layer::backend::WasmEngine>(&mut self, ctx: &mut Store<T, E>, mut transition: PackageContextTransition) -> Result<Vec<Error>> {
        ensure!(self.state == transition.state, "Transition was not created for the current context's state.");

        let to_unload = self.separate_outdated_packages(&transition);
        
        for i in transition.num_removed..transition.steps.len() {
            if let PackageContextTransitionStep::LoadPackage(step) = &mut transition.steps[i] {
                match self.load_new(ctx.as_context_mut(), step) {
                    std::result::Result::Ok(()) => continue,
                    std::result::Result::Err(err) => {
                        self.reset_from_transition(i, &transition, to_unload);
                        return Err(err);
                    }
                }
            }
            else {
                unreachable!();
            }   
        }

        self.state = Self::next_state();
        self.image = transition.image.clone();
        
        let mut errors = Vec::new();
        for (_, inst) in to_unload {
            errors.extend(inst.instance.drop(ctx).expect("Could not drop instance."));
        }

        Ok(errors)
    }

    fn load_new(&mut self, ctx: impl AsContextMut, step: &mut LoadPackage) -> Result<()> {
        let linker = step.linker.to_mut();

        for (interface, _) in step.package.component.imports().instances() {
            let exporting_package = &self.packages[interface.package().name()];
            let name = InterfaceIdentifier::new(PackageIdentifier::new(interface.package().name().clone(), exporting_package.version.clone()), interface.name());
            if let Some(exported) = exporting_package.instance.exports().instance(&name) {
                Self::fill_linker(linker, interface, exported)?;
            }
            else {
                bail!("Package {} required missing interface {interface}", step.package.id);
            }
        }

        assert!(self.packages.insert(step.package.id.name().clone(), ResolvedInstance { version: step.package.id.version().cloned(), instance: linker.instantiate(ctx, &step.package.component)? }).is_none(), "Added duplicate packages.");
        Ok(())
    }

    fn reset_from_transition(&mut self, end: usize, transition: &PackageContextTransition, to_reload: Vec<(PackageName, ResolvedInstance)>) {
        for i in transition.num_removed..end {
            if let PackageContextTransitionStep::LoadPackage(step) = &transition.steps[i] {
                self.packages.remove(step.package.id.name());
            }
            else {
                unreachable!();
            }
        }

        self.packages.extend(to_reload);
    }

    fn separate_outdated_packages(&mut self, transition: &PackageContextTransition) -> Vec<(PackageName, ResolvedInstance)> {
        let mut to_unload = Vec::with_capacity(transition.num_removed);
        for i in 0..transition.num_removed {
            if let PackageContextTransitionStep::UnloadPackage(UnloadPackage { id }) = &transition.steps[i] {
                to_unload.push(self.packages.remove_entry(id.name()).expect("Package was not available"));
            }
            else {
                unreachable!();
            }
        }
        to_unload
    }

    fn fill_linker(linker: &mut Linker, id: &InterfaceIdentifier, instance: &ExportInstance) -> Result<()> {
        let to_fill = linker.define_instance(id.clone())?;

        for (name, func) in instance.funcs() {
            to_fill.define_func(name, func)?;
        }

        for (name, resource_ty) in instance.resources() {
            to_fill.define_resource(name, resource_ty);
        }

        Ok(())
    }

    fn next_state() -> u64 {
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
        f.debug_struct("PackageContext").field("packages", &self.packages).finish()
    }
}