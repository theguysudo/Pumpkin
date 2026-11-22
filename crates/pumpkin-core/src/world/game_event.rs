use pumpkin_data::BlockDirection;
use pumpkin_data::game_event::GameEvent;
use pumpkin_data::tag::{self, Tag, Taggable};
use pumpkin_util::math::vector2::Vector2;
use pumpkin_util::math::{
    boundingbox::BoundingBox,
    position::{BlockPos, chunk_section_from_pos},
    vector3::Vector3,
};
use pumpkin_world::generation::section_coords::section_to_block;

use super::{World, entity_islands};
use crate::entity::EntityBase;

pub struct GameEventContext<'a> {
    pub source_entity: Option<&'a dyn EntityBase>,
}

pub trait GameEventListener {
    fn listener_position(&self) -> Vector3<f64>;

    fn listener_radius(&self) -> i32;

    fn handle_game_event(
        &self,
        world: &World,
        event: GameEvent,
        context: &GameEventContext<'_>,
        source_position: Vector3<f64>,
    ) -> bool;
}

fn notified_sections(center: BlockPos, radius: i32) -> (Vector3<i32>, Vector3<i32>) {
    (
        chunk_section_from_pos(&center.offset(Vector3::new(-radius, -radius, -radius))),
        chunk_section_from_pos(&center.offset(Vector3::new(radius, radius, radius))),
    )
}

/// The chunk sections whose listeners an event at `position` can notify.
#[must_use]
pub fn notified_area(position: Vector3<f64>, radius: i32) -> BoundingBox {
    let (section_min, section_max) = notified_sections(BlockPos::floored_v(position), radius);
    let block_min = |section: i32| f64::from(section_to_block(section));
    BoundingBox::new(
        Vector3::new(
            block_min(section_min.x),
            block_min(section_min.y),
            block_min(section_min.z),
        ),
        Vector3::new(
            block_min(section_max.x + 1),
            block_min(section_max.y + 1),
            block_min(section_max.z + 1),
        ),
    )
}

/// Vanilla `VibrationSystem.User.isValidVibration`: tag membership plus source-based
/// suppression (spectators, sneaking on `ignore_vibrations_sneaking` events, sources that
/// dampen vibrations). Vanilla also rejects events whose affected block state dampens
/// vibrations; Pumpkin's `GameEventContext` has no affected state yet.
pub(crate) fn is_valid_vibration(
    event: GameEvent,
    context: &GameEventContext<'_>,
    listenable_events: Tag,
) -> bool {
    if !listenable_events.0.contains(&event.name()) {
        return false;
    }
    let Some(source) = context.source_entity else {
        return true;
    };
    let ignored_while_sneaking = tag::GameEvent::MINECRAFT_IGNORE_VIBRATIONS_SNEAKING
        .0
        .contains(&event.name());
    !source.is_spectator()
        && (!source.is_stepping_carefully() || !ignored_while_sneaking)
        && !source.dampens_vibrations()
}

/// Vanilla `VibrationSystem.Listener.isOccluded`: the vibration is blocked when all six
/// rays from the source to the listener hit a block of `occludes_vibration_signals` (wool).
pub(crate) fn is_vibration_occluded(
    world: &World,
    origin: Vector3<f64>,
    destination: Vector3<f64>,
) -> bool {
    let from = BlockPos::floored_v(origin).to_centered_f64();
    let to = BlockPos::floored_v(destination).to_centered_f64();
    BlockDirection::all().iter().all(|direction| {
        let nudged_source = from + direction.to_offset().to_f64() * f64::from(1.0e-5f32);
        pumpkin_world::clip::traverse_blocks(nudged_source, to, |pos| {
            world
                .get_block(pos)
                .has_tag(&tag::Block::MINECRAFT_OCCLUDES_VIBRATION_SIGNALS)
                .then_some(())
        })
        .is_some()
    })
}

impl World {
    pub(super) fn post_game_event(
        &self,
        event: GameEvent,
        position: Vector3<f64>,
        context: &GameEventContext<'_>,
    ) {
        let radius = event.notification_radius();
        let center = BlockPos::floored_v(position);
        let (section_min, section_max) = notified_sections(center, radius);
        let area = notified_area(position, radius);
        for entity in self.entity_index.listeners(self.entities.load_full(), area) {
            let Some(mob) = entity.get_mob() else {
                continue;
            };
            if !self
                .level
                .is_chunk_loaded(&entity.get_entity().chunk_pos.load())
            {
                continue;
            }
            mob.for_each_game_event_listener(&mut |listener| {
                let listener_block = BlockPos::floored_v(listener.listener_position());
                let section = chunk_section_from_pos(&listener_block);
                let listener_radius = listener.listener_radius();
                if section.x < section_min.x
                    || section.y < section_min.y
                    || section.z < section_min.z
                    || section.x > section_max.x
                    || section.y > section_max.y
                    || section.z > section_max.z
                    || listener_block.squared_distance(&center) > listener_radius * listener_radius
                {
                    return;
                }
                entity_islands::check_reached(entity.get_entity().entity_id);
                listener.handle_game_event(self, event, context, position);
            });
        }
        self.notify_block_listeners(event, context, position, center, section_min, section_max);
    }

    /// Vanilla dispatches through the listener registry of every chunk section in range,
    /// where block entities register their listeners (`GameEventListener.Provider`); sculk
    /// sensors and calibrated sensors receive vibrations this way.
    fn notify_block_listeners(
        &self,
        event: GameEvent,
        context: &GameEventContext<'_>,
        position: Vector3<f64>,
        center: BlockPos,
        section_min: Vector3<i32>,
        section_max: Vector3<i32>,
    ) {
        // Section coordinates share the chunk X and Z of their column.
        for chunk_x in section_min.x..=section_max.x {
            for chunk_z in section_min.z..=section_max.z {
                let Some(chunk_block_entities) =
                    self.block_entities.get(&Vector2::new(chunk_x, chunk_z))
                else {
                    continue;
                };
                // Clone out of the shard guard before handling: handlers may read block
                // entities themselves.
                let block_entities: Vec<_> = chunk_block_entities.values().cloned().collect();
                drop(chunk_block_entities);
                for block_entity in block_entities {
                    let Some(listener) = block_entity.game_event_listener() else {
                        continue;
                    };
                    let listener_block = BlockPos::floored_v(listener.listener_position());
                    let section = chunk_section_from_pos(&listener_block);
                    let listener_radius = listener.listener_radius();
                    if section.x < section_min.x
                        || section.y < section_min.y
                        || section.z < section_min.z
                        || section.x > section_max.x
                        || section.y > section_max.y
                        || section.z > section_max.z
                        || listener_block.squared_distance(&center)
                            > listener_radius * listener_radius
                    {
                        continue;
                    }
                    listener.handle_game_event(self, event, context, position);
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use super::*;
    use crate::block::entities::BlockEntity;
    use crate::block::entities::calibrated_sculk_sensor::CalibratedSculkSensorBlockEntity;
    use crate::block::entities::sculk_sensor::SculkSensorBlockEntity;
    use crate::entity::ai::test_support::world;
    use pumpkin_data::Block;
    use pumpkin_data::block_properties::{
        CalibratedSculkSensorLikeProperties, SculkSensorLikeProperties, SculkSensorPhase,
    };
    use pumpkin_util::math::vector2::Vector2;
    use pumpkin_world::chunk::ChunkData;
    use pumpkin_world::world::BlockFlags;

    #[tokio::test]
    async fn block_listeners_only_receive_events_within_their_radius()
    -> Result<(), Box<dyn std::error::Error>> {
        let folder = tempfile::tempdir()?;
        let world = world(&folder);
        // Sensors require the adjacent chunks to be ticking before delivering (vanilla
        // `areAdjacentChunksTicking`), so load the 3x3 area around the origin.
        for x in -1..=1 {
            for z in -1..=1 {
                world
                    .level
                    .loaded_chunks
                    .insert(Vector2::new(x, z), ChunkData::empty_sync(x, z));
            }
        }

        // A plain sensor ten blocks from the source is outside its radius of eight; the
        // calibrated variant hears up to sixteen.
        let plain_pos = BlockPos::new(10, 64, 0);
        let calibrated_pos = BlockPos::new(11, 64, 0);
        world.set_block_state(
            &plain_pos,
            Block::SCULK_SENSOR.default_state.id,
            BlockFlags::empty(),
        );
        world.set_block_state(
            &calibrated_pos,
            Block::CALIBRATED_SCULK_SENSOR.default_state.id,
            BlockFlags::empty(),
        );
        let plain = Arc::new(SculkSensorBlockEntity::new(plain_pos));
        let calibrated = Arc::new(CalibratedSculkSensorBlockEntity::new(calibrated_pos));
        world.add_block_entity(plain.clone());
        world.add_block_entity(calibrated.clone());

        let source = BlockPos::new(0, 64, 0).to_centered_f64();
        world.post_game_event(
            GameEvent::Step,
            source,
            &GameEventContext {
                source_entity: None,
            },
        );

        for _ in 0..=16 {
            world
                .level_time
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .world_age += 1;
            plain.tick(&world);
            calibrated.tick(&world);
        }

        let plain_props =
            SculkSensorLikeProperties::from_state_id(world.get_block_state(&plain_pos).id);
        assert_eq!(
            plain_props.sculk_sensor_phase,
            SculkSensorPhase::Inactive,
            "a step event ten blocks away must not reach a plain sensor"
        );
        let calibrated_props = CalibratedSculkSensorLikeProperties::from_state_id(
            world.get_block_state(&calibrated_pos).id,
        );
        assert_eq!(
            calibrated_props.sculk_sensor_phase,
            SculkSensorPhase::Active,
            "a step event ten blocks away must reach a calibrated sensor"
        );
        world.level.shutdown().await;
        Ok(())
    }
}
