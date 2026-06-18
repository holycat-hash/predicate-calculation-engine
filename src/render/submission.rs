//! 渲染提交数据流程：把 render 的逐实体语义字段装配成一份**有序 staging 视图**，
//! 供下游后端打包成绘制 / 实例化所需的 byte rows。
//!
//! 设计立场（架构哲学：白送做满 / 有代价给接口 / 不替开发者决策）：真正的下游
//! 后端——byte layout、批次合并、residency、重型 trace / sampled imagery——是「有代价」优化，
//! 该交的是**接口 + 良好上游**而非替开发者拍板的实现。故本模块只产出「良好的渲染
//! 语义数据」：每个可见实体一条 [`RenderPacket`]（插值后的 transform、mesh / material
//! handle、动画态、淡出权重），后端据此自取所需地打包。
//!
//! 本模块也是 render → 后端的 seam：`RenderPacket` 保留语义值（兼容与可读性），
//! [`SubmissionView::instance_rows`] 则把同一帧派生成 typed / byte-layout-friendly rows。
//! 后者仍不是 driver API；它只把 transform、numeric handles、动画态和 fade 落成固定
//! 形状，让实例化渲染、byte resource 写入和 trace instance 描述消费同一份数据。
//!
//! 装配源全是 render 字段（[`RFieldId`]）：transform 经 `track` 插值落字段、
//! handle / 可见性 / 动画态经 `reaction` / `continuous` 写字段。`submit` 只读不写，是
//! render 帧的终端读出（§6.1「物化为可见集实体」的消费端：剔除把可见集收窄，提交
//! 只扫可见集）。

use std::collections::HashMap;
use std::ops::Range;

use crate::entity::{EntityTypeId, InstanceId};
use crate::value::Value;

use super::local::{LocalStore, RenderLocalId, RenderLocalTypeId};
use super::store::{RFieldId, RenderStore};

/// 某可渲染类型的字段绑定：声明哪些 render 字段填充提交包的各槽。未绑定的槽在包里
/// 取 `Null`（transform / handle 系）或默认（`anim_phase=0`、`fade=1`、可见）。
///
/// 一处声明、`submit` 处复用——避免把「字段语义」散落在每个消费点。各槽相互独立：
/// 平移走 `Vec3Lerp` track、旋转走 `Slerp` track、缩放走 `Vec3Lerp`、handle 走离散
/// render 字段，互不耦合。
#[derive(Debug, Clone, Default)]
pub struct RenderBinding {
    /// 平移（[`Value::Vec3`]，一般经 `Vec3Lerp` track）。
    pub translation: Option<RFieldId>,
    /// 旋转（[`Value::Quat`]，一般经 `Slerp` track）。
    pub rotation: Option<RFieldId>,
    /// 缩放（[`Value::Vec3`]，一般经 `Vec3Lerp` track）。
    pub scale: Option<RFieldId>,
    /// mesh handle（不透明值，离散——`Snap` track 或直接 render 字段）。
    pub mesh: Option<RFieldId>,
    /// material handle（不透明值）。
    pub material: Option<RFieldId>,
    /// 可见性（Bool）。未绑定 ⇒ 恒可见；读到 `Bool(false)` ⇒ 剔除（不进提交）。
    pub visibility: Option<RFieldId>,
    /// 动画状态 id（离散）。
    pub anim_state: Option<RFieldId>,
    /// 动画归一化进度 `[0,1)`（Float）。
    pub anim_phase: Option<RFieldId>,
    /// 淡出权重 `[0,1]`（Float）。未绑定 ⇒ 1（实心）；读到 `≤0` ⇒ 已淡尽，不进提交。
    pub fade: Option<RFieldId>,
}

/// 一个可见实体的提交包：装配好的逐实体渲染语义数据。后端把它打包成 draw / instance。
#[derive(Debug, Clone, PartialEq)]
pub struct RenderPacket {
    pub inst: InstanceId,
    /// 插值后的平移（[`Value::Vec3`]）/ 旋转（[`Value::Quat`]）/ 缩放（[`Value::Vec3`]）。
    /// 未绑定者为 `Null`（后端按缺省 TRS 处理）。三者分量独立，后端自行合成矩阵。
    pub translation: Value,
    pub rotation: Value,
    pub scale: Value,
    pub mesh: Value,
    pub material: Value,
    pub anim_state: Value,
    pub anim_phase: f64,
    /// 淡出权重 `[0,1]`：1 实心、0 淡尽（淡尽者已被排除，故恒 `>0`）。
    pub fade: f64,
}

/// 一个 render-local 临时实体的提交包。字段语义与 [`RenderPacket`] 相同，但 source id
/// 是 render 本地池的 [`RenderLocalId`]，不会伪装成 sim [`InstanceId`]。
#[derive(Debug, Clone, PartialEq)]
pub struct RenderLocalPacket {
    pub local: RenderLocalId,
    pub translation: Value,
    pub rotation: Value,
    pub scale: Value,
    pub mesh: Value,
    pub material: Value,
    pub anim_state: Value,
    pub anim_phase: f64,
    pub fade: f64,
}

/// 一个 byte-layout-friendly 的实例行：从 [`RenderPacket`] 派生出的固定形状 seam 数据。
///
/// 约定保持保守：transform 槽缺省为 identity TRS；mesh/material/anim_state 只接受
/// 非负整数 handle，其他语义值折成 0（后端可继续直接消费 [`RenderPacket`] 处理字符串
/// 或自定义 handle）。`ids[3]` 是本帧 packet 下标，trace / picking 命中后回查
/// `SubmissionView::packets[index].inst`，不把 CPU 侧 ABA generation 泄进 byte-row 约定。
/// 这层不做 draw batching、不分配 owned byte resource、不决定 residency。
#[repr(C)]
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct SubmissionInstanceRow {
    /// xyz = translation，w = fade。
    pub translation_fade: [f32; 4],
    /// xyzw = rotation quaternion。
    pub rotation: [f32; 4],
    /// xyz = scale，w = anim phase。
    pub scale_phase: [f32; 4],
    /// mesh / material / anim_state / packet_index。
    pub ids: [u32; 4],
}

/// 一行实例数据的保守分组 key。实例化渲染可用 mesh+material，动画 / trace 消费者可用
/// `anim_state` / mesh 做自己的查表；这里仍只描述数据，不决定 draw 或 trace-table 排列。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct SubmissionInstanceKey {
    pub mesh: u32,
    pub material: u32,
    pub anim_state: u32,
}

/// [`SubmissionInstanceRow`] 固定编码中的 vec4 槽。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum SubmissionInstanceSlot {
    TranslationFade,
    Rotation,
    ScalePhase,
    Ids,
}

/// [`SubmissionInstanceRow`] 的 byte layout。它描述 `to_le_bytes` 的稳定编码，而不是
/// Rust struct 的内存布局。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SubmissionInstanceLayout {
    pub stride: usize,
    pub vec4_byte_len: usize,
    pub translation_fade_offset: usize,
    pub rotation_offset: usize,
    pub scale_phase_offset: usize,
    pub ids_offset: usize,
}

impl SubmissionInstanceLayout {
    pub const VEC4_BYTE_LEN: usize = 16;

    pub const fn canonical() -> Self {
        SubmissionInstanceLayout {
            stride: SubmissionInstanceRow::BYTE_LEN,
            vec4_byte_len: Self::VEC4_BYTE_LEN,
            translation_fade_offset: 0,
            rotation_offset: Self::VEC4_BYTE_LEN,
            scale_phase_offset: Self::VEC4_BYTE_LEN * 2,
            ids_offset: Self::VEC4_BYTE_LEN * 3,
        }
    }

    pub fn row_range(&self, row: usize) -> Range<usize> {
        row.saturating_mul(self.stride)..row.saturating_add(1).saturating_mul(self.stride)
    }

    pub fn slot_range(&self, slot: SubmissionInstanceSlot) -> Range<usize> {
        let offset = match slot {
            SubmissionInstanceSlot::TranslationFade => self.translation_fade_offset,
            SubmissionInstanceSlot::Rotation => self.rotation_offset,
            SubmissionInstanceSlot::ScalePhase => self.scale_phase_offset,
            SubmissionInstanceSlot::Ids => self.ids_offset,
        };
        offset..offset.saturating_add(self.vec4_byte_len)
    }
}

impl Default for SubmissionInstanceLayout {
    fn default() -> Self {
        Self::canonical()
    }
}

/// 一个实例化 span：指向 [`SubmissionInstanceStream::rows`] 的连续区间。
///
/// `mesh/material/anim_state` 是保守的 run key：实例化绘制可用 mesh+material，动画后端
/// 可用 anim_state 选 clip/palette，trace 后端可用 mesh 选 geometry source。span 只描述当前 rows
/// 顺序里的连续 run，不排序、不合并非相邻 run，不替后端决定 execution layout / trace table / byte resource
/// residency。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SubmissionInstanceSpan {
    pub mesh: u32,
    pub material: u32,
    pub anim_state: u32,
    pub first: u32,
    pub count: u32,
}

impl SubmissionInstanceSpan {
    pub fn key(&self) -> SubmissionInstanceKey {
        SubmissionInstanceKey {
            mesh: self.mesh,
            material: self.material,
            anim_state: self.anim_state,
        }
    }

    pub fn range(&self) -> Range<usize> {
        let first = self.first as usize;
        first..first.saturating_add(self.count as usize)
    }

    pub fn byte_range(&self) -> Range<usize> {
        let rows = self.range();
        rows.start * SubmissionInstanceRow::BYTE_LEN..rows.end * SubmissionInstanceRow::BYTE_LEN
    }
}

/// 从 [`SubmissionView`] 派生出的 instance stream：`rows` 可直接写入 staging byte image，
/// `spans` 描述相同 key 的连续 run。rows 与 [`SubmissionView::packets`] 完全同序；每行
/// `ids[3]` 保留原始 packet index，所以 trace / picking / debug 能回查语义 packet。
#[derive(Debug, Clone, Default, PartialEq)]
pub struct SubmissionInstanceStream {
    pub rows: Vec<SubmissionInstanceRow>,
    pub spans: Vec<SubmissionInstanceSpan>,
}

impl SubmissionInstanceStream {
    pub fn len(&self) -> usize {
        self.rows.len()
    }

    pub fn is_empty(&self) -> bool {
        self.rows.is_empty()
    }

    pub fn clear(&mut self) {
        self.rows.clear();
        self.spans.clear();
    }

    pub fn byte_len(&self) -> usize {
        self.rows.len() * SubmissionInstanceRow::BYTE_LEN
    }

    /// 把 rows 编码成连续 little-endian bytes。调用方可把这段数据写入 staging byte image；
    /// 本方法不绑定任何外部 API，也不使用 unsafe layout transmute。
    pub fn instance_bytes(&self) -> Vec<u8> {
        let mut bytes = Vec::with_capacity(self.byte_len());
        self.fill_instance_bytes(&mut bytes);
        bytes
    }

    /// 复用调用方提供的 byte Vec，按 [`SubmissionInstanceRow::BYTE_LEN`] 连续写入 rows。
    pub fn fill_instance_bytes(&self, out: &mut Vec<u8>) {
        out.clear();
        out.reserve(self.byte_len());
        for row in &self.rows {
            row.append_le_bytes(out);
        }
    }
}

impl Default for SubmissionInstanceRow {
    fn default() -> Self {
        SubmissionInstanceRow {
            translation_fade: [0.0, 0.0, 0.0, 1.0],
            rotation: [0.0, 0.0, 0.0, 1.0],
            scale_phase: [1.0, 1.0, 1.0, 0.0],
            ids: [0, 0, 0, u32::MAX],
        }
    }
}

impl SubmissionInstanceRow {
    pub const BYTE_LEN: usize = 64;
    pub const AFFINE3X4_BYTE_LEN: usize = 48;
    pub const LAYOUT: SubmissionInstanceLayout = SubmissionInstanceLayout::canonical();

    /// 从语义 packet 派生固定行。`packet_index` 是回查语义 packet / `InstanceId` 的唯一桥。
    pub fn from_packet(packet: &RenderPacket, packet_index: u32) -> Self {
        row_from_semantic(
            &packet.translation,
            &packet.rotation,
            &packet.scale,
            &packet.mesh,
            &packet.material,
            &packet.anim_state,
            packet.anim_phase,
            packet.fade,
            packet_index,
        )
    }

    /// 从 render-local 语义 packet 派生固定行。`packet_index` 回查
    /// [`LocalSubmissionView::packets`]。
    pub fn from_local_packet(packet: &RenderLocalPacket, packet_index: u32) -> Self {
        row_from_semantic(
            &packet.translation,
            &packet.rotation,
            &packet.scale,
            &packet.mesh,
            &packet.material,
            &packet.anim_state,
            packet.anim_phase,
            packet.fade,
            packet_index,
        )
    }

    pub fn packet_index(&self) -> Option<usize> {
        (self.ids[3] != u32::MAX).then_some(self.ids[3] as usize)
    }

    pub fn translation(&self) -> [f32; 3] {
        [
            self.translation_fade[0],
            self.translation_fade[1],
            self.translation_fade[2],
        ]
    }

    pub fn fade(&self) -> f32 {
        self.translation_fade[3]
    }

    pub fn scale(&self) -> [f32; 3] {
        [
            self.scale_phase[0],
            self.scale_phase[1],
            self.scale_phase[2],
        ]
    }

    pub fn anim_phase(&self) -> f32 {
        self.scale_phase[3]
    }

    pub fn mesh_handle(&self) -> u32 {
        self.ids[0]
    }

    pub fn material_handle(&self) -> u32 {
        self.ids[1]
    }

    pub fn anim_state(&self) -> u32 {
        self.ids[2]
    }

    pub fn key(&self) -> SubmissionInstanceKey {
        SubmissionInstanceKey {
            mesh: self.mesh_handle(),
            material: self.material_handle(),
            anim_state: self.anim_state(),
        }
    }

    pub fn span_key(&self) -> (u32, u32, u32) {
        let key = self.key();
        (key.mesh, key.material, key.anim_state)
    }

    /// 固定 little-endian 编码：translation_fade、rotation、scale_phase、ids 依次展开。
    /// 这比直接暴露 Rust struct 内存布局更窄、更可测，也避免 `unsafe`。
    pub fn to_le_bytes(&self) -> [u8; Self::BYTE_LEN] {
        let mut bytes = [0u8; Self::BYTE_LEN];
        let mut offset = 0;
        for v in self.translation_fade {
            write_f32(v, &mut bytes, &mut offset);
        }
        for v in self.rotation {
            write_f32(v, &mut bytes, &mut offset);
        }
        for v in self.scale_phase {
            write_f32(v, &mut bytes, &mut offset);
        }
        for v in self.ids {
            write_u32(v, &mut bytes, &mut offset);
        }
        bytes
    }

    pub fn from_le_bytes(bytes: &[u8]) -> Option<Self> {
        if bytes.len() < Self::BYTE_LEN {
            return None;
        }
        let mut offset = 0;
        let mut read_f32 = || {
            let value = f32::from_le_bytes(bytes[offset..offset + 4].try_into().ok()?);
            offset += 4;
            Some(value)
        };
        let translation_fade = [read_f32()?, read_f32()?, read_f32()?, read_f32()?];
        let rotation = [read_f32()?, read_f32()?, read_f32()?, read_f32()?];
        let scale_phase = [read_f32()?, read_f32()?, read_f32()?, read_f32()?];
        let mut read_u32 = || {
            let value = u32::from_le_bytes(bytes[offset..offset + 4].try_into().ok()?);
            offset += 4;
            Some(value)
        };
        let ids = [read_u32()?, read_u32()?, read_u32()?, read_u32()?];
        Some(SubmissionInstanceRow {
            translation_fade,
            rotation,
            scale_phase,
            ids,
        })
    }

    pub fn append_le_bytes(&self, out: &mut Vec<u8>) {
        out.extend_from_slice(&self.to_le_bytes());
    }

    pub fn copy_le_bytes_to(&self, out: &mut [u8]) -> bool {
        if out.len() < Self::BYTE_LEN {
            return false;
        }
        out[..Self::BYTE_LEN].copy_from_slice(&self.to_le_bytes());
        true
    }

    /// 只导出 affine 3x4 的 row-major little-endian bytes，供 trace instance transform 或
    /// CPU staging builder 复用。它仍然不表达任何具体光追 API 的 instance layout。
    pub fn affine3x4_le_bytes(&self) -> [u8; Self::AFFINE3X4_BYTE_LEN] {
        let mut bytes = [0u8; Self::AFFINE3X4_BYTE_LEN];
        let mut offset = 0;
        for row in self.affine3x4() {
            for v in row {
                write_f32(v, &mut bytes, &mut offset);
            }
        }
        bytes
    }

    pub fn affine3x4_from_le_bytes(bytes: &[u8]) -> Option<[[f32; 4]; 3]> {
        if bytes.len() < Self::AFFINE3X4_BYTE_LEN {
            return None;
        }
        let mut offset = 0;
        let mut read_f32 = || {
            let value = f32::from_le_bytes(bytes[offset..offset + 4].try_into().ok()?);
            offset += 4;
            Some(value)
        };
        Some([
            [read_f32()?, read_f32()?, read_f32()?, read_f32()?],
            [read_f32()?, read_f32()?, read_f32()?, read_f32()?],
            [read_f32()?, read_f32()?, read_f32()?, read_f32()?],
        ])
    }

    pub fn copy_affine3x4_le_bytes_to(&self, out: &mut [u8]) -> bool {
        if out.len() < Self::AFFINE3X4_BYTE_LEN {
            return false;
        }
        out[..Self::AFFINE3X4_BYTE_LEN].copy_from_slice(&self.affine3x4_le_bytes());
        true
    }

    /// 行主序 3x4 affine 矩阵：`R * S` 加平移。列缩放让非均匀 scale 与旋转组合保持常规
    /// TRS 语义。
    pub fn affine3x4(&self) -> [[f32; 4]; 3] {
        let [x, y, z, w] = self.rotation;
        let [sx, sy, sz, _phase] = self.scale_phase;
        let xx = x * x;
        let yy = y * y;
        let zz = z * z;
        let xy = x * y;
        let xz = x * z;
        let yz = y * z;
        let xw = x * w;
        let yw = y * w;
        let zw = z * w;

        [
            [
                (1.0 - 2.0 * (yy + zz)) * sx,
                (2.0 * (xy - zw)) * sy,
                (2.0 * (xz + yw)) * sz,
                self.translation_fade[0],
            ],
            [
                (2.0 * (xy + zw)) * sx,
                (1.0 - 2.0 * (xx + zz)) * sy,
                (2.0 * (yz - xw)) * sz,
                self.translation_fade[1],
            ],
            [
                (2.0 * (xz - yw)) * sx,
                (2.0 * (yz + xw)) * sy,
                (1.0 - 2.0 * (xx + yy)) * sz,
                self.translation_fade[2],
            ],
        ]
    }
}

/// 一个 render 帧的提交视图：按类型注册序、类型内按行序排列的提交包（稳定序）。
/// 批次 / 排序 / byte packing 交给后端（不替开发者决策）。
#[derive(Debug, Clone, Default)]
pub struct SubmissionView {
    pub packets: Vec<RenderPacket>,
}

impl SubmissionView {
    pub fn len(&self) -> usize {
        self.packets.len()
    }

    pub fn is_empty(&self) -> bool {
        self.packets.is_empty()
    }

    pub fn iter(&self) -> std::slice::Iter<'_, RenderPacket> {
        self.packets.iter()
    }

    /// 从语义提交包派生 typed / byte-layout-friendly 实例行。顺序与 `packets` 完全一致，
    /// 因而不会悄悄替后端做排序或 batching。
    pub fn instance_rows(&self) -> Vec<SubmissionInstanceRow> {
        let mut rows = Vec::with_capacity(self.packets.len());
        self.fill_instance_rows(&mut rows);
        rows
    }

    /// 复用调用方提供的 Vec，避免每帧派生 seam 数据时强制重新分配。
    pub fn fill_instance_rows(&self, out: &mut Vec<SubmissionInstanceRow>) {
        out.clear();
        out.reserve(self.packets.len());
        for (i, packet) in self.packets.iter().enumerate() {
            out.push(SubmissionInstanceRow::from_packet(
                packet,
                u32::try_from(i).unwrap_or(u32::MAX),
            ));
        }
    }

    /// 派生 instance stream。row 顺序与 `SubmissionView::packets` 一致；spans 只是对
    /// 相邻相同 key 的连续 run 做索引。
    pub fn instance_stream(&self) -> SubmissionInstanceStream {
        let mut stream = SubmissionInstanceStream::default();
        self.fill_instance_stream(&mut stream);
        stream
    }

    /// 复用调用方持有的 stream 分配，生成 rows + contiguous spans。
    pub fn fill_instance_stream(&self, out: &mut SubmissionInstanceStream) {
        out.clear();
        self.fill_instance_rows(&mut out.rows);
        rebuild_spans(&out.rows, &mut out.spans);
    }
}

/// render-local 临时实体提交视图。它与 [`SubmissionView`] 分开返回，避免 local id
/// 与 sim [`InstanceId`] 混用；byte-row 派生仍复用同一固定 layout。
#[derive(Debug, Clone, Default)]
pub struct LocalSubmissionView {
    pub packets: Vec<RenderLocalPacket>,
}

impl LocalSubmissionView {
    pub fn len(&self) -> usize {
        self.packets.len()
    }

    pub fn is_empty(&self) -> bool {
        self.packets.is_empty()
    }

    pub fn iter(&self) -> std::slice::Iter<'_, RenderLocalPacket> {
        self.packets.iter()
    }

    pub fn instance_rows(&self) -> Vec<SubmissionInstanceRow> {
        let mut rows = Vec::with_capacity(self.packets.len());
        self.fill_instance_rows(&mut rows);
        rows
    }

    pub fn fill_instance_rows(&self, out: &mut Vec<SubmissionInstanceRow>) {
        out.clear();
        out.reserve(self.packets.len());
        for (i, packet) in self.packets.iter().enumerate() {
            out.push(SubmissionInstanceRow::from_local_packet(
                packet,
                u32::try_from(i).unwrap_or(u32::MAX),
            ));
        }
    }

    pub fn instance_stream(&self) -> SubmissionInstanceStream {
        let mut stream = SubmissionInstanceStream::default();
        self.fill_instance_stream(&mut stream);
        stream
    }

    pub fn fill_instance_stream(&self, out: &mut SubmissionInstanceStream) {
        out.clear();
        self.fill_instance_rows(&mut out.rows);
        rebuild_spans(&out.rows, &mut out.spans);
    }
}

/// 从一组渲染绑定装配提交视图。逐类型按注册序、类型内按 render 行序扫描存活
/// （含淡出中的）实体，读绑定字段，剔除不可见 / 已淡尽者。
///
/// `visible`（[`super::RenderRuntime`] 本帧算出的可见集）若为 `Some` 且含某类型，则该
/// 类型只扫可见集（空间剔除收窄候选，§6.1 的 render 对偶）；否则该类型扫全部存活。空间
/// 剔除与逐实体的 `visibility` / `fade` 字段正交叠加——前者收窄候选，后者再细筛。
pub(super) fn assemble(
    store: &RenderStore,
    renderables: &[(EntityTypeId, RenderBinding)],
    visible: Option<&HashMap<EntityTypeId, Vec<InstanceId>>>,
) -> SubmissionView {
    let mut packets = vec![];
    for (ty, b) in renderables {
        // 剔除类型（可见集含该 ty）只扫可见集；否则稠密扫存活（含淡出中）。
        let insts: Vec<InstanceId> = match visible.and_then(|m| m.get(ty)) {
            Some(vis) => vis.clone(),
            None => {
                let mut v = vec![];
                store.for_each_live(*ty, |i| v.push(i));
                v
            }
        };
        for inst in insts {
            if !store.is_present(inst) {
                continue;
            }
            // 剔除：绑定了可见性且读到 false。未绑定 / Null / true 一律可见（宽容默认）。
            if let Some(vf) = b.visibility
                && matches!(store.read_render(inst, vf), Value::Bool(false))
            {
                continue;
            }
            let fade = match b.fade {
                None => 1.0,
                Some(f) => match store.read_render(inst, f).as_f64() {
                    Some(v) if v.is_finite() => v.clamp(0.0, 1.0),
                    _ => 0.0,
                },
            };
            if fade <= 0.0 {
                continue; // 已淡尽：本帧不画（行将由 render 寿命管理回收）。
            }
            packets.push(RenderPacket {
                inst,
                translation: read_opt(store, inst, b.translation),
                rotation: read_opt(store, inst, b.rotation),
                scale: read_opt(store, inst, b.scale),
                mesh: read_opt(store, inst, b.mesh),
                material: read_opt(store, inst, b.material),
                anim_state: read_opt(store, inst, b.anim_state),
                anim_phase: b
                    .anim_phase
                    .map_or(0.0, |f| store.read_render(inst, f).as_f64().unwrap_or(0.0)),
                fade,
            });
        }
    }
    SubmissionView { packets }
}

pub(super) fn assemble_local(
    store: &LocalStore,
    renderables: &[(RenderLocalTypeId, RenderBinding)],
) -> LocalSubmissionView {
    let mut packets = vec![];
    for (ty, b) in renderables {
        let mut ids = vec![];
        store.for_each_live(*ty, |id| ids.push(id));
        for local in ids {
            if !store.is_present(local) {
                continue;
            }
            if let Some(vf) = b.visibility
                && matches!(store.read(local, vf), Value::Bool(false))
            {
                continue;
            }
            let fade = match b.fade {
                None => 1.0,
                Some(f) => match store.read(local, f).as_f64() {
                    Some(v) if v.is_finite() => v.clamp(0.0, 1.0),
                    _ => 0.0,
                },
            };
            if fade <= 0.0 {
                continue;
            }
            packets.push(RenderLocalPacket {
                local,
                translation: read_opt_local(store, local, b.translation),
                rotation: read_opt_local(store, local, b.rotation),
                scale: read_opt_local(store, local, b.scale),
                mesh: read_opt_local(store, local, b.mesh),
                material: read_opt_local(store, local, b.material),
                anim_state: read_opt_local(store, local, b.anim_state),
                anim_phase: b
                    .anim_phase
                    .map_or(0.0, |f| store.read(local, f).as_f64().unwrap_or(0.0)),
                fade,
            });
        }
    }
    LocalSubmissionView { packets }
}

fn read_opt(store: &RenderStore, inst: InstanceId, f: Option<RFieldId>) -> Value {
    f.map_or(Value::Null, |f| store.read_render(inst, f))
}

fn read_opt_local(store: &LocalStore, local: RenderLocalId, f: Option<RFieldId>) -> Value {
    f.map_or(Value::Null, |f| store.read(local, f))
}

fn rebuild_spans(rows: &[SubmissionInstanceRow], out: &mut Vec<SubmissionInstanceSpan>) {
    out.clear();
    if rows.is_empty() {
        return;
    }
    let mut start = 0usize;
    let mut key = rows[0].key();
    for i in 1..=rows.len() {
        let next_key = rows.get(i).map(SubmissionInstanceRow::key);
        if next_key != Some(key) {
            out.push(SubmissionInstanceSpan {
                mesh: key.mesh,
                material: key.material,
                anim_state: key.anim_state,
                first: u32::try_from(start).unwrap_or(u32::MAX),
                count: u32::try_from(i - start).unwrap_or(u32::MAX),
            });
            if let Some(k) = next_key {
                start = i;
                key = k;
            }
        }
    }
}

fn write_f32<const N: usize>(v: f32, out: &mut [u8; N], offset: &mut usize) {
    out[*offset..*offset + 4].copy_from_slice(&v.to_le_bytes());
    *offset += 4;
}

fn write_u32<const N: usize>(v: u32, out: &mut [u8; N], offset: &mut usize) {
    out[*offset..*offset + 4].copy_from_slice(&v.to_le_bytes());
    *offset += 4;
}

#[allow(clippy::too_many_arguments)]
fn row_from_semantic(
    translation_value: &Value,
    rotation_value: &Value,
    scale_value: &Value,
    mesh: &Value,
    material: &Value,
    anim_state: &Value,
    anim_phase: f64,
    fade: f64,
    packet_index: u32,
) -> SubmissionInstanceRow {
    let default = SubmissionInstanceRow::default();
    let translation = vec3_f32(translation_value).unwrap_or([
        default.translation_fade[0],
        default.translation_fade[1],
        default.translation_fade[2],
    ]);
    let scale = vec3_f32(scale_value).unwrap_or([
        default.scale_phase[0],
        default.scale_phase[1],
        default.scale_phase[2],
    ]);
    SubmissionInstanceRow {
        translation_fade: [
            translation[0],
            translation[1],
            translation[2],
            finite_clamped_f32(fade, 1.0),
        ],
        rotation: quat_f32(rotation_value).unwrap_or(default.rotation),
        scale_phase: [scale[0], scale[1], scale[2], finite_f32(anim_phase, 0.0)],
        ids: [
            numeric_handle(mesh),
            numeric_handle(material),
            numeric_handle(anim_state),
            packet_index,
        ],
    }
}

fn numeric_handle(v: &Value) -> u32 {
    match v {
        Value::Int(i) => u32::try_from(*i).unwrap_or(0),
        _ => 0,
    }
}

fn finite_f32(v: f64, default: f32) -> f32 {
    if v.is_finite() { v as f32 } else { default }
}

fn finite_clamped_f32(v: f64, default: f32) -> f32 {
    if v.is_finite() {
        (v as f32).clamp(0.0, 1.0)
    } else {
        default
    }
}

fn vec3_f32(v: &Value) -> Option<[f32; 3]> {
    let a = v.as_vec3()?;
    if a.iter().all(|x| x.is_finite()) {
        Some([a[0] as f32, a[1] as f32, a[2] as f32])
    } else {
        None
    }
}

fn quat_f32(v: &Value) -> Option<[f32; 4]> {
    let q = v.as_quat()?;
    if q.iter().any(|x| !x.is_finite()) {
        return None;
    }
    let len = (q[0] * q[0] + q[1] * q[1] + q[2] * q[2] + q[3] * q[3]).sqrt();
    if len < 1e-12 {
        return None;
    }
    Some([
        (q[0] / len) as f32,
        (q[1] / len) as f32,
        (q[2] / len) as f32,
        (q[3] / len) as f32,
    ])
}
