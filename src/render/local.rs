//! render-local temporary entities: render-owned, pooled, and invisible to sim.
//!
//! Local entities are for visual-only residents such as particles and floating
//! text. They never enter the simulation schema, write log, predicates, or
//! shared sidecar lifecycle. The render runtime owns their ids, fields, pooling,
//! and destruction.

use crate::column::Column;
use crate::genslots::GenSlots;
use crate::value::Value;

use super::store::RFieldId;

/// render-local entity type id. Its namespace is separate from sim [`EntityTypeId`].
///
/// [`EntityTypeId`]: crate::entity::EntityTypeId
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct RenderLocalTypeId(pub u32);

/// render-local entity id. Freed ids are pooled and reused with a bumped
/// generation so stale handles cannot address a new resident.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct RenderLocalId {
    pub ty: RenderLocalTypeId,
    pub id: u32,
    pub(crate) generation: u64,
}

/// Field definition for a render-local type. Local fields live in the render
/// namespace, so their ids are [`RFieldId`] rather than sim [`FieldId`].
///
/// [`FieldId`]: crate::entity::FieldId
#[derive(Debug, Clone)]
pub struct RenderLocalFieldDef {
    pub name: String,
    pub default: Value,
}

impl RenderLocalFieldDef {
    pub fn new(name: &str, default: Value) -> Self {
        RenderLocalFieldDef {
            name: name.to_string(),
            default,
        }
    }
}

#[derive(Debug, Clone)]
pub(crate) enum RenderLocalCommand {
    Spawn {
        ty: RenderLocalTypeId,
        init: Vec<(RFieldId, Value)>,
    },
    Destroy(RenderLocalId),
}

struct LocalTypeStore {
    name: String,
    fields: Vec<RenderLocalFieldDef>,
    cols: Vec<Column>,
    /// Slot == local id. `GenSlots` tracks current live generation.
    slots: GenSlots,
    /// Current/next generation per local id. Incremented on release.
    generations: Vec<u64>,
    free_ids: Vec<u32>,
}

impl LocalTypeStore {
    fn ensure_row(&mut self, id: usize) {
        if id < self.slots.len() {
            return;
        }
        let n = id + 1;
        for (fi, col) in self.cols.iter_mut().enumerate() {
            col.resize(n, &self.fields[fi].default);
        }
        self.slots.grow_to(n);
    }

    fn reset_row(&mut self, id: usize) {
        for (fi, col) in self.cols.iter_mut().enumerate() {
            col.set(id, self.fields[fi].default.clone());
        }
    }

    fn clear_row(&mut self, id: usize) {
        for col in &mut self.cols {
            col.clear_row(id);
        }
    }
}

pub(crate) struct LocalStore {
    types: Vec<LocalTypeStore>,
}

impl LocalStore {
    pub(crate) fn new() -> Self {
        LocalStore { types: vec![] }
    }

    pub(crate) fn add_type(
        &mut self,
        name: &str,
        fields: Vec<RenderLocalFieldDef>,
    ) -> RenderLocalTypeId {
        let ty = RenderLocalTypeId(self.types.len() as u32);
        let cols = fields
            .iter()
            .map(|f| Column::with_default(&f.default, 0))
            .collect();
        self.types.push(LocalTypeStore {
            name: name.to_string(),
            fields,
            cols,
            slots: GenSlots::new(),
            generations: vec![],
            free_ids: vec![],
        });
        ty
    }

    pub(crate) fn has_type(&self, ty: RenderLocalTypeId) -> bool {
        self.types.get(ty.0 as usize).is_some()
    }

    pub(crate) fn has_field(&self, ty: RenderLocalTypeId, f: RFieldId) -> bool {
        self.types
            .get(ty.0 as usize)
            .is_some_and(|t| (f.0 as usize) < t.fields.len())
    }

    pub(crate) fn field(&self, ty: RenderLocalTypeId, name: &str) -> Result<RFieldId, String> {
        let t = self
            .types
            .get(ty.0 as usize)
            .ok_or_else(|| format!("无 render-local 类型 id {}", ty.0))?;
        t.fields
            .iter()
            .position(|f| f.name == name)
            .map(|i| RFieldId(i as u32))
            .ok_or_else(|| format!("render-local 类型 {} 无字段 {name}", t.name))
    }

    pub(crate) fn spawn(
        &mut self,
        ty: RenderLocalTypeId,
        init: Vec<(RFieldId, Value)>,
    ) -> Result<RenderLocalId, String> {
        let t = self
            .types
            .get_mut(ty.0 as usize)
            .ok_or_else(|| format!("无 render-local 类型 id {}", ty.0))?;
        for &(f, _) in &init {
            if (f.0 as usize) >= t.fields.len() {
                return Err(format!(
                    "render-local spawn 初始化不存在字段 {}.{}",
                    ty.0, f.0
                ));
            }
        }
        let id = match t.free_ids.pop() {
            Some(id) => id,
            None => {
                let id = t.generations.len() as u32;
                t.generations.push(0);
                id
            }
        };
        let row = id as usize;
        t.ensure_row(row);
        let generation = t.generations[row];
        t.slots.activate(row, generation);
        t.reset_row(row);
        for (f, v) in init {
            t.cols[f.0 as usize].set(row, v);
        }
        Ok(RenderLocalId { ty, id, generation })
    }

    pub(crate) fn destroy(&mut self, id: RenderLocalId) -> bool {
        let Some(t) = self.types.get_mut(id.ty.0 as usize) else {
            return false;
        };
        let row = id.id as usize;
        if !t.slots.matches(row, id.generation) {
            return false;
        }
        t.slots.kill(row);
        t.clear_row(row);
        t.generations[row] = id
            .generation
            .checked_add(1)
            .expect("RenderLocalId generation exhausted");
        t.free_ids.push(id.id);
        true
    }

    pub(crate) fn is_present(&self, id: RenderLocalId) -> bool {
        self.types
            .get(id.ty.0 as usize)
            .is_some_and(|t| t.slots.matches(id.id as usize, id.generation))
    }

    pub(crate) fn read(&self, id: RenderLocalId, f: RFieldId) -> Value {
        let Some(t) = self.types.get(id.ty.0 as usize) else {
            return Value::Null;
        };
        let row = id.id as usize;
        if !t.slots.matches(row, id.generation) {
            return Value::Null;
        }
        t.cols.get(f.0 as usize).map_or(Value::Null, |c| c.get(row))
    }

    pub(crate) fn write(&mut self, id: RenderLocalId, f: RFieldId, v: Value) {
        let Some(t) = self.types.get_mut(id.ty.0 as usize) else {
            return;
        };
        let row = id.id as usize;
        if !t.slots.matches(row, id.generation) {
            return;
        }
        if let Some(col) = t.cols.get_mut(f.0 as usize) {
            col.set(row, v);
        }
    }

    pub(crate) fn for_each_live(&self, ty: RenderLocalTypeId, mut f: impl FnMut(RenderLocalId)) {
        let Some(t) = self.types.get(ty.0 as usize) else {
            return;
        };
        t.slots.for_each_live(|slot, generation| {
            f(RenderLocalId {
                ty,
                id: slot as u32,
                generation,
            });
        });
    }

    pub(crate) fn live_count(&self, ty: RenderLocalTypeId) -> usize {
        let mut n = 0;
        self.for_each_live(ty, |_| n += 1);
        n
    }
}
