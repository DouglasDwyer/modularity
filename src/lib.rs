#![allow(warnings)]

use anyhow::*;
use fxhash::*;
use semver::*;
use std::mem::*;
use std::sync::*;
use topological_sort::*;
use wasm_component_layer::*;

#[derive(PartialEq, Eq)]
struct PartialVersionRef<'a>(Option<&'a Version>);

impl<'a> PartialVersionRef<'a> {    
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
pub struct ResolvedPackage {
    pub id: PackageIdentifier,
    pub component: Component
}

#[derive(Clone, Debug)]
struct PackageResolutionInfo {
    pub version: Option<Version>,
    pub exact: bool,
    pub state: PackageResolutionState
}

#[derive(Clone, Debug)]
enum PackageResolutionState {
    Unresolved(u16),
    Resolved(Component),
    InGraph(Component),
}

#[derive(Clone, Debug)]
pub struct UnresolvedPackage {
    exact: bool,
    id: PackageIdentifier,
    selected: Option<(PackageIdentifier, Component)>
}

impl UnresolvedPackage {
    pub fn id(&self) -> &PackageIdentifier {
        &self.id
    }

    pub fn exact(&self) -> bool {
        self.exact
    }

    pub fn is_resolved(&self) -> bool {
        self.selected.is_some()
    }

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

#[derive(Clone, Debug)]
pub struct PackageResolver {
    graph: TopologicalSort<PackageName>,
    chosen_packages: FxHashMap<PackageName, PackageResolutionInfo>,
    top_level_packages: FxHashMap<PackageName, Option<Version>>,
    unresolved_packages: Vec<UnresolvedPackage>,
    unresolved_packages_backing: Vec<UnresolvedPackage>,
    to_resolve: Vec<PackageIdentifier>,
    linker: Arc<Linker>
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
            graph: TopologicalSort::default(),
            chosen_packages: FxHashMap::default(),
            top_level_packages,
            unresolved_packages: Vec::with_capacity(to_resolve.len()),
            unresolved_packages_backing: Vec::default(),
            to_resolve,
            linker: Arc::new(linker)
        }
    }

    pub fn unresolved(&mut self) -> &mut [UnresolvedPackage] {
        &mut self.unresolved_packages
    }

    pub fn resolve(mut self) -> Result<Vec<ResolvedPackage>, PackageResolverError> {
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
                            self.graph = TopologicalSort::default();
                            self.unresolved_packages.clear();
                            self.to_resolve.clear();
                            self.to_resolve.extend(self.top_level_packages.iter().map(|(a, b)| PackageIdentifier::new(a.clone(), b.clone())));
    
                            for pkg in self.chosen_packages.values_mut() {
                                if let PackageResolutionState::InGraph(x) = &mut pkg.state {
                                    pkg.state = PackageResolutionState::Resolved(x.clone());
                                }
                            }
                        },
                    }
                }
                else if let PackageResolutionState::Resolved(x) = &mut info.state {
                    self.graph.insert(next.name().clone());
                    
                    for interface in x.imports().instances().filter_map(|(x, _)| self.linker.instance(x).is_none().then_some(x)) {
                        self.graph.add_dependency(interface.package().name().clone(), next.name().clone());
                        self.to_resolve.push(interface.package().clone());
                    }

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

    fn into_package_topology(mut self) -> Result<Vec<ResolvedPackage>, PackageResolverError> {
        let mut res = Vec::with_capacity(self.graph.len());

        while let Some(name) = self.graph.pop() {
            let chosen = &self.chosen_packages[&name];
            if let PackageResolutionState::InGraph(x) = &chosen.state {
                res.push(ResolvedPackage { id: PackageIdentifier::new(name.clone(), chosen.version.clone()), component: x.clone() });
            }
            else {
                unreachable!();
            }
        }

        if self.graph.len() > 0 {
            std::result::Result::Err(PackageResolverError::CyclicPackageDependency())
        }
        else {
            std::result::Result::Ok(res)
        }
    }

    fn clear_unresolved(&mut self) {
        for resolved in self.unresolved_packages.drain(..) {
            if let Some((id, pkg)) = resolved.selected {
                let info = self.chosen_packages.get_mut(id.name()).expect("Package was not in map.");
                info.version = id.version().cloned();
                info.state = PackageResolutionState::Resolved(pkg);
                self.to_resolve.push(id);
            }
            else {
                let index = self.unresolved_packages_backing.len() as u16;
                self.chosen_packages.get_mut(resolved.id.name()).expect("Package was not in map.").state = PackageResolutionState::Unresolved(index);
                self.unresolved_packages_backing.push(resolved);
            }
        }
        
        swap(&mut self.unresolved_packages, &mut self.unresolved_packages_backing);
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

#[derive(Clone, Debug)]
pub enum PackageResolverError {
    CyclicPackageDependency(),
    IncompatibleVersions(PackageName, Option<Version>, Option<Version>),
    MissingPackages(PackageResolver)
}