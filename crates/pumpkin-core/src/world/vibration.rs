use std::sync::Arc;

use pumpkin_data::game_event::{GameEvent, PositionSourceType};
use pumpkin_data::particle::Particle;
use pumpkin_data::tag::Tag;
use pumpkin_nbt::compound::NbtCompound;
use pumpkin_nbt::tag::NbtTag;
use pumpkin_protocol::codec::var_int::VarInt;
use pumpkin_protocol::java::client::play::CParticle;
use pumpkin_protocol::ser::WritingError;
use pumpkin_util::math::position::{BlockPos, chunk_section_from_pos};
use pumpkin_util::math::vector2::Vector2;
use pumpkin_util::math::vector3::Vector3;
use uuid::Uuid;

use super::World;
use super::game_event::{GameEventContext, is_valid_vibration, is_vibration_occluded};
use crate::entity::EntityBase;

/// Players within this distance receive vibration particles, like vanilla
/// `ServerLevel.sendParticles`.
const PARTICLE_SEND_DISTANCE: f64 = 32.0;

/// Vanilla `VibrationSystem.getRedstoneStrengthForDistance`: the redstone power a listener
/// emits grows weaker with the distance to the vibration source.
#[must_use]
pub fn redstone_strength_for_distance(distance: f32, listener_radius: i32) -> u8 {
    let power_scale = 15.0 / f64::from(listener_radius);
    let power = 15.0 - (power_scale * f64::from(distance)).floor();
    power.max(1.0) as u8
}

/// A vibration that reached its listener, ready for
/// [`VibrationUser::on_receive_vibration`].
pub struct VibrationDelivery {
    pub event: GameEvent,
    pub origin: BlockPos,
    pub source: Option<Arc<dyn EntityBase>>,
    pub receiving_distance: f32,
}

/// Vanilla `net.minecraft.world.level.gameevent.vibrations.VibrationInfo`: one vibration
/// heading towards a listener.
#[derive(Clone)]
pub struct VibrationInfo {
    pub event: GameEvent,
    pub distance: f32,
    pub pos: Vector3<f64>,
    pub source: Option<Uuid>,
}

impl VibrationInfo {
    fn write_nbt(&self) -> NbtCompound {
        let mut nbt = NbtCompound::new();
        nbt.put_string("game_event", format!("minecraft:{}", self.event.name()));
        nbt.put_float("distance", self.distance);
        nbt.put_list(
            "pos",
            vec![
                NbtTag::Double(self.pos.x),
                NbtTag::Double(self.pos.y),
                NbtTag::Double(self.pos.z),
            ],
        );
        if let Some(source) = &self.source {
            nbt.put_uuid("source", *source);
        }
        nbt
    }

    fn read_nbt(nbt: &NbtCompound) -> Option<Self> {
        let event = GameEvent::from_name(nbt.get_string("game_event")?)?;
        let distance = nbt.get_float("distance")?;
        let pos = nbt.get_list("pos")?;
        if pos.len() != 3 || !distance.is_finite() || distance < 0.0 {
            return None;
        }
        let pos = Vector3::new(
            pos[0].extract_double()?,
            pos[1].extract_double()?,
            pos[2].extract_double()?,
        );
        if !pos.x.is_finite() || !pos.y.is_finite() || !pos.z.is_finite() {
            return None;
        }
        Some(Self {
            event,
            distance,
            pos,
            source: nbt.get_uuid("source"),
        })
    }
}

/// Vanilla `VibrationSelector`: keeps a single candidate; on the same tick the nearest
/// vibration wins, and equal distances are broken by the higher frequency.
pub struct VibrationSelector {
    current: Option<(VibrationInfo, i64)>,
}

impl VibrationSelector {
    #[must_use]
    pub const fn new() -> Self {
        Self { current: None }
    }

    fn add_candidate(&mut self, vibration: VibrationInfo, tick: i64) {
        if self.should_replace_vibration(&vibration, tick) {
            self.current = Some((vibration, tick));
        }
    }

    /// Vanilla `VibrationSelector.shouldReplaceVibration`.
    fn should_replace_vibration(&self, vibration: &VibrationInfo, tick: i64) -> bool {
        let Some((previous, previous_tick)) = &self.current else {
            return true;
        };
        if tick != *previous_tick {
            return false;
        }
        if vibration.distance < previous.distance {
            return true;
        }
        if vibration.distance > previous.distance {
            return false;
        }
        vibration.event.vibration_frequency() > previous.event.vibration_frequency()
    }

    /// Vanilla `VibrationSelector.chosenCandidate`: the candidate becomes deliverable on the
    /// tick after it was scheduled.
    fn chosen_candidate(&self, time: i64) -> Option<&VibrationInfo> {
        self.current
            .as_ref()
            .filter(|(_, tick)| *tick < time)
            .map(|(vibration, _)| vibration)
    }

    const fn start_over(&mut self) {
        self.current = None;
    }
}

impl Default for VibrationSelector {
    fn default() -> Self {
        Self::new()
    }
}

/// Vanilla `VibrationSystem.Data`: the pending candidate, in-flight vibration and travel
/// time of one listener. Persisted as the block entity's `listener` tag.
pub struct VibrationData {
    pub current: Option<VibrationInfo>,
    selector: VibrationSelector,
    travel_time: i32,
    reload_particle: bool,
}

impl VibrationData {
    #[must_use]
    pub const fn new() -> Self {
        Self {
            current: None,
            selector: VibrationSelector::new(),
            travel_time: 0,
            reload_particle: false,
        }
    }

    /// Vanilla `VibrationSystem.Listener.handleGameEvent`.
    pub fn handle_game_event(
        &mut self,
        world: &World,
        user: &dyn VibrationUser,
        event: GameEvent,
        context: &GameEventContext<'_>,
        source_position: Vector3<f64>,
    ) -> bool {
        if self.current.is_some() || !is_valid_vibration(event, context, user.listenable_events()) {
            return false;
        }
        if !user.can_receive_vibration(world, BlockPos::floored_v(source_position), event, context)
        {
            return false;
        }
        let destination = user.listener_position();
        if is_vibration_occluded(world, source_position, destination) {
            return false;
        }
        self.selector.add_candidate(
            VibrationInfo {
                event,
                distance: (source_position - destination).length() as f32,
                pos: source_position,
                source: context
                    .source_entity
                    .map(|source| source.get_entity().entity_uuid),
            },
            world.get_world_age(),
        );
        true
    }

    /// Vanilla `VibrationSystem.Listener.forceScheduleVibration`: schedules a vibration
    /// without the validity and occlusion checks, as `SculkSensorBlock.stepOn` does.
    pub fn force_schedule(
        &mut self,
        world: &World,
        user: &dyn VibrationUser,
        event: GameEvent,
        context: &GameEventContext<'_>,
        origin: Vector3<f64>,
    ) {
        let destination = user.listener_position();
        self.selector.add_candidate(
            VibrationInfo {
                event,
                distance: (origin - destination).length() as f32,
                pos: origin,
                source: context
                    .source_entity
                    .map(|source| source.get_entity().entity_uuid),
            },
            world.get_world_age(),
        );
    }

    /// Vanilla `VibrationSystem.Ticker.tick`, which the block entity ticker runs every tick.
    /// Selects a candidate, travels towards the listener and returns the vibration once it
    /// arrives; the caller delivers it without holding the listener's lock, because
    /// activation emits game events that can re-enter this listener (vanilla processes
    /// those with the in-flight vibration already cleared).
    pub fn tick(&mut self, world: &World, user: &dyn VibrationUser) -> Option<VibrationDelivery> {
        if self.current.is_none()
            && let Some(vibration) = self
                .selector
                .chosen_candidate(world.get_world_age())
                .cloned()
        {
            self.travel_time = vibration.distance.floor() as i32;
            Self::send_vibration_particle(
                world,
                vibration.pos,
                user.listener_position(),
                self.travel_time,
            );
            self.selector.start_over();
            self.current = Some(vibration);
        }
        self.current.as_ref()?;
        self.try_reload_vibration_particle(world, user);
        self.travel_time = (self.travel_time - 1).max(0);
        if self.travel_time > 0 {
            return None;
        }
        // Vanilla `VibrationSystem.Ticker.receiveVibration`. A failed adjacency check keeps
        // the vibration so it is retried on the next tick.
        let destination = user.listener_block_position();
        if user.requires_adjacent_chunks_ticking() && !adjacent_chunks_loaded(world, &destination) {
            return None;
        }
        let vibration = self.current.take()?;
        let origin = BlockPos::floored_v(vibration.pos);
        let receiving_distance = f64::from(origin.squared_distance(&destination)).sqrt() as f32;
        let source = vibration.source.and_then(|uuid| {
            let entities = world.entities.load_full();
            world.entity_index.by_uuid(&entities, uuid)
        });
        Some(VibrationDelivery {
            event: vibration.event,
            origin,
            source,
            receiving_distance,
        })
    }

    /// Vanilla `VibrationSystem.Ticker.tryReloadVibrationParticle`.
    fn try_reload_vibration_particle(&mut self, world: &World, user: &dyn VibrationUser) {
        if !self.reload_particle {
            return;
        }
        let Some(vibration) = &self.current else {
            self.reload_particle = false;
            return;
        };
        let initial_travel = vibration.distance.floor() as i32;
        let alpha = 1.0 - f64::from(self.travel_time) / f64::from(initial_travel);
        let pos = vibration.pos.lerp(&user.listener_position(), alpha);
        if Self::send_vibration_particle(world, pos, user.listener_position(), self.travel_time) {
            self.reload_particle = false;
        }
    }

    /// Vanilla `ServerLevel.sendParticles` of the vibration particle.
    fn send_vibration_particle(
        world: &World,
        origin: Vector3<f64>,
        destination: Vector3<f64>,
        delay: i32,
    ) -> bool {
        let Ok(payload) = vibration_payload(BlockPos::floored_v(destination), delay) else {
            return false;
        };
        let packet = CParticle::new(
            false,
            false,
            origin,
            Vector3::new(0.0, 0.0, 0.0),
            0.0,
            1,
            VarInt(i32::from(Particle::Vibration.to_id())),
            &payload,
        );
        let mut sent = false;
        for player in world.players.load().iter() {
            if player.position().squared_distance_to_vec(&origin)
                <= PARTICLE_SEND_DISTANCE * PARTICLE_SEND_DISTANCE
            {
                player.try_send_client_packet(&packet);
                sent = true;
            }
        }
        sent
    }

    /// NBT shape of vanilla `VibrationSystem.Data`: `event`, `selector` (`event`, `tick`)
    /// and `event_delay`.
    #[must_use]
    pub fn write_nbt(&self) -> NbtCompound {
        let mut nbt = NbtCompound::new();
        if let Some(vibration) = &self.current {
            nbt.put_compound("event", vibration.write_nbt());
        }
        let mut selector = NbtCompound::new();
        selector.put_long(
            "tick",
            self.selector.current.as_ref().map_or(-1, |(_, tick)| *tick),
        );
        if let Some((vibration, _)) = &self.selector.current {
            selector.put_compound("event", vibration.write_nbt());
        }
        nbt.put_compound("selector", selector);
        nbt.put_int("event_delay", self.travel_time);
        nbt
    }

    #[must_use]
    pub fn read_nbt(nbt: &NbtCompound) -> Self {
        let Some(selector) = nbt.get_compound("selector") else {
            return Self::new();
        };
        let Some(tick) = selector.get_long("tick") else {
            return Self::new();
        };
        let delay = nbt.get_int("event_delay").unwrap_or(0);
        if delay < 0 {
            return Self::new();
        }
        Self {
            current: nbt.get_compound("event").and_then(VibrationInfo::read_nbt),
            selector: VibrationSelector {
                current: selector
                    .get_compound("event")
                    .and_then(VibrationInfo::read_nbt)
                    .map(|vibration| (vibration, tick)),
            },
            travel_time: delay,
            reload_particle: true,
        }
    }
}

impl Default for VibrationData {
    fn default() -> Self {
        Self::new()
    }
}

/// Vanilla `VibrationSystem.User`: the per-listener half of the vibration system.
pub trait VibrationUser {
    /// Vanilla `VibrationSystem.User.getPositionSource`, which sculk listeners implement as
    /// a `BlockPositionSource` of their own block.
    fn listener_block_position(&self) -> BlockPos;

    fn listener_position(&self) -> Vector3<f64> {
        self.listener_block_position().to_centered_f64()
    }

    /// Vanilla `VibrationSystem.User.getListenerRadius`.
    fn listener_radius(&self) -> i32;

    /// Vanilla `VibrationSystem.User.getListenableEvents`.
    fn listenable_events(&self) -> Tag;

    /// Vanilla `VibrationSystem.User.canReceiveVibration`.
    fn can_receive_vibration(
        &self,
        world: &World,
        pos: BlockPos,
        event: GameEvent,
        context: &GameEventContext<'_>,
    ) -> bool;

    /// Vanilla `VibrationSystem.User.onReceiveVibration`.
    fn on_receive_vibration(
        &self,
        world: &Arc<World>,
        pos: BlockPos,
        event: GameEvent,
        source: Option<Arc<dyn EntityBase>>,
        receiving_distance: f32,
    );

    /// Vanilla `VibrationSystem.User.requiresAdjacentChunksToBeTicking`.
    fn requires_adjacent_chunks_ticking(&self) -> bool {
        false
    }
}

/// Vanilla `SculkSensorBlock.stepOn`: an entity standing on the listener schedules a step
/// vibration without the validity and occlusion checks.
pub fn schedule_step_on(
    world: &Arc<World>,
    user: &dyn VibrationUser,
    data: &mut VibrationData,
    entity: &dyn EntityBase,
) {
    let context = GameEventContext {
        source_entity: Some(entity),
    };
    if !user.can_receive_vibration(
        world,
        user.listener_block_position(),
        GameEvent::Step,
        &context,
    ) {
        return;
    }
    data.force_schedule(
        world,
        user,
        GameEvent::Step,
        &context,
        entity.get_entity().pos.load(),
    );
}

/// Vanilla `VibrationSystem.areAdjacentChunksTicking`, approximated with loaded chunks.
fn adjacent_chunks_loaded(world: &World, pos: &BlockPos) -> bool {
    let section = chunk_section_from_pos(pos);
    for x in -1..=1 {
        for z in -1..=1 {
            if !world
                .level
                .is_chunk_loaded(&Vector2::new(section.x + x, section.z + z))
            {
                return false;
            }
        }
    }
    true
}

/// Particle payload of vanilla `VibrationParticleOption` with a `BlockPositionSource`
/// destination: position source type, packed block position, arrival delay.
fn vibration_payload(destination: BlockPos, delay: i32) -> Result<Vec<u8>, WritingError> {
    let mut payload = Vec::new();
    VarInt(PositionSourceType::Block.to_id()).encode(&mut payload)?;
    payload.extend_from_slice(&destination.as_long().to_be_bytes());
    VarInt(delay).encode(&mut payload)?;
    Ok(payload)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn vibration(event: GameEvent, distance: f32) -> VibrationInfo {
        VibrationInfo {
            event,
            distance,
            pos: Vector3::new(0.5, 64.5, 0.5),
            source: None,
        }
    }

    #[test]
    fn selector_prefers_the_nearest_candidate_of_the_same_tick() {
        let mut selector = VibrationSelector::new();
        selector.add_candidate(vibration(GameEvent::Step, 5.0), 10);
        selector.add_candidate(vibration(GameEvent::Step, 3.0), 10);
        selector.add_candidate(vibration(GameEvent::Step, 7.0), 10);
        assert_eq!(selector.chosen_candidate(11).map(|v| v.distance), Some(3.0));
    }

    #[test]
    fn selector_keeps_a_candidate_from_an_older_tick() {
        // Vanilla `shouldReplaceVibration`: candidates scheduled on different ticks never
        // replace each other.
        let mut selector = VibrationSelector::new();
        selector.add_candidate(vibration(GameEvent::Step, 5.0), 10);
        selector.add_candidate(vibration(GameEvent::Step, 1.0), 11);
        assert_eq!(selector.chosen_candidate(12).map(|v| v.distance), Some(5.0));
    }

    #[test]
    fn selector_tie_breaks_by_higher_frequency() {
        let mut selector = VibrationSelector::new();
        selector.add_candidate(vibration(GameEvent::Step, 4.0), 10);
        selector.add_candidate(vibration(GameEvent::BlockPlace, 4.0), 10);
        assert!(matches!(
            selector.chosen_candidate(11).map(|v| v.event),
            Some(GameEvent::BlockPlace)
        ));
    }

    #[test]
    fn chosen_candidate_waits_for_the_next_tick() {
        let mut selector = VibrationSelector::new();
        selector.add_candidate(vibration(GameEvent::Step, 4.0), 10);
        assert!(selector.chosen_candidate(10).is_none());
        assert!(selector.chosen_candidate(11).is_some());
    }

    #[test]
    fn vibration_data_nbt_round_trips() {
        let mut data = VibrationData::new();
        let mut selector = VibrationSelector::new();
        selector.add_candidate(vibration(GameEvent::BlockDestroy, 6.5), 42);
        data.selector = selector;
        data.travel_time = 3;
        let nbt = data.write_nbt();
        let restored = VibrationData::read_nbt(&nbt);
        assert_eq!(restored.travel_time, 3);
        assert!(restored.reload_particle);
        assert!(restored.current.is_none());
        assert_eq!(
            restored.selector.chosen_candidate(43).map(|v| v.distance),
            Some(6.5)
        );
        assert!(matches!(
            restored.selector.chosen_candidate(43).map(|v| v.event),
            Some(GameEvent::BlockDestroy)
        ));
    }

    #[test]
    fn redstone_strength_follows_vanillas_distance_scale() {
        // max(1, 15 - floor(15 / 8 * distance)) for the plain sensor's radius of 8.
        assert_eq!(redstone_strength_for_distance(0.0, 8), 15);
        assert_eq!(redstone_strength_for_distance(4.0, 8), 8);
        assert_eq!(redstone_strength_for_distance(8.0, 8), 1);
        assert_eq!(redstone_strength_for_distance(7.999, 8), 1);
    }
}
