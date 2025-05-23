use crate::{
    Data, Error,
    data::{EnforcementAction, GuildConfig, NotificationMethod, PendingEnforcement, Warning},
    enforcement::EnforcementCheckRequest,
};
use chrono::{Duration, Utc};
use poise::serenity_prelude as serenity;
use poise::serenity_prelude::{Colour, CreateEmbed, CreateMessage, Mentionable, Timestamp, User};
use poise::{Context, command};
use tracing::{error, info, warn};
use uuid::Uuid;

/// Basic ping command
/// This command is used to check if the bot is responsive.
#[command(slash_command, guild_only, ephemeral)]
pub async fn ping(ctx: Context<'_, Data, Error>) -> Result<(), Error> {
    ctx.say("Pong!").await?;
    Ok(())
}

/// Summon the daemon to judge a user's voice behavior
#[command(
    slash_command,
    ephemeral,
    guild_only,
    required_permissions = "KICK_MEMBERS|BAN_MEMBERS|MUTE_MEMBERS|DEAFEN_MEMBERS|MODERATE_MEMBERS",
    required_bot_permissions = "KICK_MEMBERS|BAN_MEMBERS|MUTE_MEMBERS|DEAFEN_MEMBERS|MODERATE_MEMBERS",
    default_member_permissions = "KICK_MEMBERS|BAN_MEMBERS|MUTE_MEMBERS|DEAFEN_MEMBERS|MODERATE_MEMBERS"
)]
pub async fn summon_daemon(
    ctx: Context<'_, Data, Error>,
    #[description = "User to warn"] user: User,
    #[description = "Reason for warning"] reason: String,
) -> Result<(), Error> {
    ctx.defer().await?;
    let guild_id = ctx
        .guild_id()
        .ok_or("This command must be used in a guild")?;

    // Get guild configuration
    let guild_config = ctx.data().guild_configs.get(&guild_id).map_or_else(
        || GuildConfig {
            guild_id: guild_id.get(),
            chaos_factor: 0.3, // Default chaos factor
            ..Default::default()
        },
        |entry| entry.clone(),
    );

    // Record this warning in the user's warning state
    let user_id = user.id.get();
    let mod_id = ctx.author().id.get();
    let state =
        ctx.data()
            .add_to_user_warning_state(user_id, guild_id.get(), reason.clone(), mod_id);

    // Calculate the warning score
    let score = ctx.data().calculate_warning_score(user_id, guild_id.get());

    // Determine if enforcement should be triggered
    // Threshold is 3.0 (roughly 3 recent warnings)
    const WARNING_THRESHOLD: f64 = 3.0;

    // Add randomness based on the chaos factor
    let random_factor: f64 = {
        let mut rng = rand::thread_rng();
        rand::Rng::gen_range(&mut rng, 0.0..guild_config.chaos_factor as f64)
    };
    let adjusted_score = score + random_factor;

    let enforce = adjusted_score > WARNING_THRESHOLD;
    let enforcement_action = if state.pending_enforcement.is_some() {
        // Use the pending enforcement that was set on first warning
        state.pending_enforcement.clone()
    } else if state.warning_timestamps.len() == 1 {
        // This is the first warning, set a pending enforcement
        // Default to VoiceMute for 5 minutes
        let enforcement =
            guild_config
                .default_enforcement
                .unwrap_or(EnforcementAction::VoiceMute {
                    duration: Some(300),
                });

        // Store the pending enforcement in the user state
        let key = format!("{}:{}", user_id, guild_id.get());
        let mut updated_state = state.clone();
        updated_state.pending_enforcement = Some(enforcement.clone());
        ctx.data().user_warning_states.insert(key, updated_state);

        Some(enforcement)
    } else {
        None
    };

    // Notify the user via the enforcement log channel
    if let Some(log_channel_id) = guild_config.enforcement_log_channel_id {
        let channel_id = serenity::ChannelId::new(log_channel_id);
        let user_mention = user.mention();
        let mod_mention = ctx.author().mention();

        let warning_count = state.warning_timestamps.len();

        let mut embed = serenity::CreateEmbed::new()
            .title("⚠️ Voice Channel Warning")
            .description(format!(
                "{} has received a voice channel warning",
                user_mention
            ))
            .field("Reason", &reason, false)
            .field("Issued By", mod_mention.to_string(), true)
            .field("Total Warnings", warning_count.to_string(), true)
            .colour(serenity::Colour::GOLD)
            .timestamp(serenity::Timestamp::now());

        // If this might lead to enforcement, indicate that
        if let Some(ref action) = enforcement_action {
            if state.warning_timestamps.len() == 1 {
                // This is the first warning, indicate what will happen
                let action_desc = match action {
                    EnforcementAction::VoiceMute { duration } => {
                        format!("Voice mute for {} seconds", duration.unwrap_or(300))
                    }
                    EnforcementAction::VoiceDeafen { duration } => {
                        format!("Voice deafen for {} seconds", duration.unwrap_or(300))
                    }
                    EnforcementAction::VoiceDisconnect { .. } => "Voice disconnect".to_string(),
                    EnforcementAction::Mute { duration } => {
                        format!("Server mute for {} seconds", duration.unwrap_or(300))
                    }
                    EnforcementAction::Ban { duration } => {
                        format!("Ban for {} seconds", duration.unwrap_or(86400))
                    }
                    EnforcementAction::Kick { .. } => "Kick".to_string(),
                    EnforcementAction::None => "No action".to_string(),
                    EnforcementAction::VoiceChannelHaunt {
                        teleport_count,
                        interval,
                        return_to_origin,
                        ..
                    } => {
                        format!(
                            "Voice channel haunting: {} teleports over {} seconds{}",
                            teleport_count.unwrap_or(3),
                            interval.unwrap_or(10),
                            if return_to_origin.unwrap_or(true) {
                                " (with return)"
                            } else {
                                " (no return)"
                            }
                        )
                    }
                };

                embed = embed.field(
                    "🚨 If behavior continues:",
                    format!(
                        "After ~{} more warnings, the user will receive: **{}**",
                        WARNING_THRESHOLD as u32 - 1,
                        action_desc
                    ),
                    false,
                );
            } else if enforce {
                // Enforcement is happening now
                embed = embed
                    .title("🚫 Voice Channel Enforcement")
                    .colour(serenity::Colour::RED)
                    .field(
                        "⚠️ Threshold Reached",
                        "Enforcement action is being applied",
                        false,
                    );
            }
        }

        let message = serenity::CreateMessage::new().embed(embed);
        let _ = channel_id.send_message(&ctx.http(), message).await;
    }

    // If enforcing, create the enforcement
    if enforce && enforcement_action.is_some() {
        let warning_id = uuid::Uuid::new_v4().to_string();
        let enforcement_id = uuid::Uuid::new_v4().to_string();
        let now = chrono::Utc::now().to_rfc3339();

        // Create a warning record
        let warning = Warning {
            id: warning_id.clone(),
            user_id,
            issuer_id: mod_id,
            guild_id: guild_id.get(),
            reason: format!(
                "Automatic enforcement after multiple voice warnings: {}",
                reason
            ),
            timestamp: now.clone(),
            notification_method: NotificationMethod::PublicWithMention,
            enforcement: enforcement_action.clone(),
        };

        // Store warning
        ctx.data().warnings.insert(warning_id.clone(), warning);

        // Create pending enforcement
        if let Some(action) = enforcement_action {
            let execute_at = match &action {
                EnforcementAction::Ban { duration }
                | EnforcementAction::Mute { duration }
                | EnforcementAction::VoiceMute { duration }
                | EnforcementAction::VoiceDeafen { duration } => {
                    chrono::Utc::now() + chrono::Duration::seconds(duration.unwrap_or(0) as i64)
                }
                EnforcementAction::Kick { delay }
                | EnforcementAction::VoiceDisconnect { delay } => {
                    chrono::Utc::now() + chrono::Duration::seconds(delay.unwrap_or(0) as i64)
                }
                EnforcementAction::VoiceChannelHaunt { interval, .. } => {
                    chrono::Utc::now() + chrono::Duration::seconds(interval.unwrap_or(0) as i64)
                }
                EnforcementAction::None => chrono::Utc::now(),
            };

            let pending = PendingEnforcement {
                id: enforcement_id.clone(),
                warning_id,
                user_id,
                guild_id: guild_id.get(),
                action,
                execute_at: execute_at.to_rfc3339(),
                executed: false,
            };

            ctx.data()
                .pending_enforcements
                .insert(enforcement_id, pending);

            // Notify the enforcement task
            if let Some(tx) = &*ctx.data().enforcement_tx {
                let _ = tx
                    .send(EnforcementCheckRequest::CheckUser {
                        user_id,
                        guild_id: guild_id.get(),
                    })
                    .await;
            }
        }
    }

    // Save data
    if let Err(e) = ctx.data().save().await {
        error!("Failed to save data after VC thumbs down: {}", e);
    }

    // Respond to the moderator
    let response = if enforce {
        format!(
            "Thumbs down recorded for {} with reason: {}. Enforcement action applied due to multiple warnings.",
            user.name, reason
        )
    } else if state.warning_timestamps.len() == 1 {
        format!(
            "First thumbs down recorded for {} with reason: {}. Further warnings may trigger enforcement.",
            user.name, reason
        )
    } else {
        format!(
            "Thumbs down recorded for {} with reason: {}. Current warning count: {}",
            user.name,
            reason,
            state.warning_timestamps.len()
        )
    };

    ctx.say(response).await?;
    Ok(())
}

/// Warn a user for inappropriate behavior
#[allow(clippy::too_many_lines)]
#[command(
    slash_command,
    ephemeral,
    guild_only,
    required_permissions = "KICK_MEMBERS|BAN_MEMBERS|MUTE_MEMBERS|DEAFEN_MEMBERS|MODERATE_MEMBERS",
    required_bot_permissions = "KICK_MEMBERS|BAN_MEMBERS|MUTE_MEMBERS|DEAFEN_MEMBERS|MODERATE_MEMBERS",
    default_member_permissions = "KICK_MEMBERS|BAN_MEMBERS|MUTE_MEMBERS|DEAFEN_MEMBERS|MODERATE_MEMBERS"
)]
pub async fn warn(
    ctx: Context<'_, Data, Error>,
    #[description = "User to warn"] user: User,
    #[description = "Reason for warning"] reason: String,
    #[description = "Notification method (DM or Public)"] notification: Option<String>,
    #[description = "Action to take (mute, ban, kick, voicemute, voicedeafen, voicedisconnect)"]
    action: Option<String>,
    #[description = "Duration in minutes for mute/ban/voicemute/voicedeafen, delay for kick/voicedisconnect"]
    duration_minutes: Option<u64>,
) -> Result<(), Error> {
    ctx.defer().await?;
    let guild_id = ctx
        .guild_id()
        .ok_or("This command must be used in a guild")?;

    // Get guild configuration
    let guild_config = ctx.data().guild_configs.get(&guild_id).map_or_else(
        || GuildConfig {
            guild_id: guild_id.get(),
            ..Default::default()
        },
        |entry| entry.clone(),
    );

    // Determine notification method
    let notification_method = match notification.as_deref() {
        Some("public" | "Public") => NotificationMethod::PublicWithMention,
        Some("dm" | "DM") => NotificationMethod::DirectMessage,
        _ => guild_config.default_notification_method,
    };

    // Determine enforcement action
    let duration = duration_minutes.map(|d| d * 60);
    let enforcement = match action.as_deref() {
        Some("mute" | "Mute") => Some(EnforcementAction::Mute { duration }),
        Some("ban" | "Ban") => Some(EnforcementAction::Ban { duration }),
        Some("kick" | "Kick") => Some(EnforcementAction::Kick { delay: duration }),
        Some("voicemute" | "VoiceMute") => Some(EnforcementAction::VoiceMute { duration }),
        Some("voicedeafen" | "VoiceDeafen") => Some(EnforcementAction::VoiceDeafen { duration }),
        Some("voicedisconnect" | "VoiceDisconnect") => {
            Some(EnforcementAction::VoiceDisconnect { delay: duration })
        }
        _ => guild_config.default_enforcement,
    };

    warn!("Enforcement action: {enforcement:?}");

    // Create warning
    let warning_id = Uuid::new_v4().to_string();
    let now = Utc::now().to_rfc3339();
    let warning = Warning {
        id: warning_id.clone(),
        user_id: user.id.get(),
        issuer_id: ctx.author().id.get(),
        guild_id: guild_id.get(),
        reason,
        timestamp: now.clone(),
        notification_method,
        enforcement: enforcement.clone(),
    };

    // Store warning
    ctx.data()
        .warnings
        .insert(warning_id.clone(), warning.clone());

    // Create pending enforcement if applicable
    if let Some(action) = enforcement {
        let enforcement_id = Uuid::new_v4().to_string();
        let execute_at = match &action {
            EnforcementAction::Ban { duration }
            | EnforcementAction::Mute { duration }
            | EnforcementAction::VoiceMute { duration }
            | EnforcementAction::VoiceDeafen { duration } => {
                Utc::now() + Duration::seconds(duration.unwrap_or(0) as i64)
            }
            EnforcementAction::Kick { delay } | EnforcementAction::VoiceDisconnect { delay } => {
                Utc::now() + Duration::seconds(delay.unwrap_or(0) as i64)
            }
            EnforcementAction::VoiceChannelHaunt { interval, .. } => {
                Utc::now() + Duration::seconds(interval.unwrap_or(0) as i64)
            }
            EnforcementAction::None => unreachable!(),
        };

        let pending = PendingEnforcement {
            id: enforcement_id.clone(),
            warning_id,
            user_id: user.id.get(),
            guild_id: guild_id.get(),
            action,
            execute_at: execute_at.to_rfc3339(),
            executed: false,
        };
        info!("Pending enforcement created: {pending:?}");
        ctx.data()
            .pending_enforcements
            .insert(enforcement_id, pending);
        info!(
            "Pending enforcements: {:?}",
            ctx.data().pending_enforcements
        );
    }

    // Notify user based on notification method
    match warning.notification_method {
        NotificationMethod::DirectMessage => {
            if let Ok(channel) = user.create_dm_channel(&ctx.http()).await {
                let embed = CreateEmbed::new()
                    .title("Warning Received")
                    .description(format!(
                        "You have been warned in {} for: {}",
                        ctx.guild().unwrap().name,
                        warning.reason
                    ))
                    .colour(Colour::RED)
                    .timestamp(Timestamp::now());

                let message = CreateMessage::new().embed(embed);
                let _ = channel.send_message(&ctx.http(), message).await;
            }
        }
        NotificationMethod::PublicWithMention => {
            let content = format!(
                "{} You have been warned for: {}",
                user.mention(),
                warning.reason
            );
            let embed = CreateEmbed::new()
                .title("Warning Issued")
                .description(&content)
                .colour(Colour::RED)
                .timestamp(Timestamp::now());

            ctx.send(poise::CreateReply::default().embed(embed)).await?;
        }
    }

    // Log the warning
    info!(
        target: crate::COMMAND_TARGET,
        command = "warn",
        guild_id = %guild_id.get(),
        user_id = %user.id.get(),
        issuer_id = %ctx.author().id.get(),
        reason = %warning.reason,
        event = "warning_issued",
        "Warning issued to user"
    );

    // Save data
    if let Err(e) = ctx.data().save().await {
        error!("Failed to save data after warning: {}", e);
    }

    info!(
        target: crate::COMMAND_TARGET,
        command = "warn",
        guild_id = %guild_id.get(),
        user_id = %user.id.get(),
        issuer_id = %ctx.author().id.get(),
        reason = %warning.reason,
        event = "warning_saved",
        "Warning saved to database"
    );

    // If there's an immediate action, notify the enforcement task
    if let Some(action) = &warning.enforcement {
        info!(
            target: crate::COMMAND_TARGET,
            command = "warn",
            guild_id = %guild_id.get(),
            user_id = %user.id.get(),
            issuer_id = %ctx.author().id.get(),
            reason = %warning.reason,
            event = "immediate_enforcement_check",
            enforcement_action = ?action,
            "Immediate enforcement action detected"
        );

        // Check if this is an immediate action
        let is_immediate = match action {
            EnforcementAction::Kick { delay } | EnforcementAction::VoiceDisconnect { delay } => {
                delay.is_none() || delay.is_some_and(|d| d == 0)
            }
            EnforcementAction::Mute { duration }
            | EnforcementAction::VoiceMute { duration }
            | EnforcementAction::VoiceDeafen { duration }
            | EnforcementAction::Ban { duration } => {
                duration.is_none() || duration.is_some_and(|d| d == 0)
            }
            EnforcementAction::VoiceChannelHaunt { interval, .. } => {
                interval.is_none() || interval.is_some_and(|d| d == 0)
            }
            EnforcementAction::None => false,
        };

        if is_immediate {
            info!(
                target: crate::COMMAND_TARGET,
                command = "warn",
                guild_id = %guild_id.get(),
                user_id = %user.id.get(),
                issuer_id = %ctx.author().id.get(),
                event = "immediate_enforcement_request",
                "Sending immediate enforcement check request"
            );
            // For immediate actions, notify the enforcement task
            if let Some(tx) = &*ctx.data().enforcement_tx {
                let _ = tx
                    .send(EnforcementCheckRequest::CheckUser {
                        user_id: user.id.get(),
                        guild_id: guild_id.get(),
                    })
                    .await;
            }
        } else {
            warn!("Enforcement action is not immediate: {action:?}");
            // Non-immediate actions will be handled by the regular check interval
        }
    }

    ctx.say(format!("Warned {} for: {}", user.name, warning.reason))
        .await?;
    Ok(())
}

/// Set the altar where the daemon will send its messages
#[command(
    slash_command,
    guild_only,
    ephemeral,
    required_permissions = "ADMINISTRATOR"
)]
pub async fn daemon_altar(
    ctx: Context<'_, Data, Error>,
    #[description = "Channel to use for enforcement logs"] channel: serenity::Channel,
) -> Result<(), Error> {
    let guild_id = ctx
        .guild_id()
        .ok_or("This command must be used in a guild")?;

    // Get current guild config or create default
    let mut guild_config = ctx.data().guild_configs.get(&guild_id).map_or_else(
        || GuildConfig {
            guild_id: guild_id.get(),
            chaos_factor: 0.3, // Default chaos factor
            ..Default::default()
        },
        |entry| entry.clone(),
    );

    // Update the config with the new channel ID
    let channel_id = channel.id();
    guild_config.enforcement_log_channel_id = Some(channel_id.get());

    // Save the updated config
    ctx.data().guild_configs.insert(guild_id, guild_config);

    // Save data
    if let Err(e) = ctx.data().save().await {
        error!(
            "Failed to save data after setting enforcement log channel: {}",
            e
        );
        ctx.say("Failed to save configuration. Check logs for details.")
            .await?;
        return Ok(());
    }

    // Send a test message to verify permissions
    let embed = serenity::CreateEmbed::new()
        .title("✅ Enforcement Log Channel Set")
        .description("This channel will now receive all enforcement notifications.")
        .colour(serenity::Colour::DARK_GREEN)
        .timestamp(serenity::Timestamp::now());

    let message = serenity::CreateMessage::new().embed(embed);

    match channel_id.send_message(&ctx.http(), message).await {
        Ok(_) => {
            ctx.say(format!(
                "Successfully set {} as the enforcement log channel!",
                channel.mention()
            ))
            .await?;
        }
        Err(e) => {
            error!("Failed to send test message to channel: {}", e);
            ctx.say(format!("⚠️ Set {} as the enforcement log channel, but couldn't send a test message. Please check bot permissions in that channel.", channel.mention()))
                .await?;
        }
    }

    Ok(())
}

/// Perform a ritual to adjust the daemon's chaos level
#[command(
    slash_command,
    guild_only,
    ephemeral,
    required_permissions = "ADMINISTRATOR"
)]
pub async fn chaos_ritual(
    ctx: Context<'_, Data, Error>,
    #[description = "Chaos factor (0.0-1.0) where higher means more random"] factor: f32,
) -> Result<(), Error> {
    let guild_id = ctx
        .guild_id()
        .ok_or("This command must be used in a guild")?;

    if !(0.0..=1.0).contains(&factor) {
        ctx.say("Chaos factor must be between 0.0 and 1.0").await?;
        return Ok(());
    }

    // Get current guild config or create default
    let mut guild_config = ctx.data().guild_configs.get(&guild_id).map_or_else(
        || GuildConfig {
            guild_id: guild_id.get(),
            ..Default::default()
        },
        |entry| entry.clone(),
    );

    // Update the chaos factor
    guild_config.chaos_factor = factor;

    // Save the updated config
    ctx.data().guild_configs.insert(guild_id, guild_config);

    // Save data
    if let Err(e) = ctx.data().save().await {
        error!("Failed to save data after setting chaos factor: {}", e);
        ctx.say("Failed to save configuration. Check logs for details.")
            .await?;
        return Ok(());
    }

    let response = format!("Chaos factor set to {}. ", factor);
    let description = if factor < 0.2 {
        "Enforcement will be mostly predictable."
    } else if factor < 0.5 {
        "Enforcement will have some randomness."
    } else if factor < 0.8 {
        "Enforcement will be quite unpredictable."
    } else {
        "Enforcement will be highly chaotic!"
    };

    ctx.say(format!("{}{}", response, description)).await?;
    Ok(())
}

/// Appease the daemon to cancel a pending punishment
#[command(
    slash_command,
    guild_only,
    ephemeral,
    required_permissions = "ADMINISTRATOR"
)]
pub async fn appease(
    ctx: Context<'_, Data, Error>,
    #[description = "User whose enforcement to cancel"] user: User,
    #[description = "Specific enforcement ID to cancel (optional)"] enforcement_id: Option<String>,
) -> Result<(), Error> {
    let guild_id = ctx
        .guild_id()
        .ok_or("This command must be used in a guild")?;
    let user_id = user.id.get();
    let mut canceled = false;
    let mut response = String::new();

    // Find pending enforcements for this user in this guild
    let mut pending_to_cancel = Vec::new();
    for entry in &ctx.data().pending_enforcements {
        let pending = entry.value();
        if pending.user_id == user_id && pending.guild_id == guild_id.get() && !pending.executed {
            if let Some(ref eid) = enforcement_id {
                if pending.id == *eid {
                    pending_to_cancel.push(pending.id.clone());
                    break;
                }
            } else {
                pending_to_cancel.push(pending.id.clone());
            }
        }
    }

    // Cancel the found enforcements
    for id in pending_to_cancel {
        if let Some(mut pending) = ctx.data().pending_enforcements.get_mut(&id) {
            pending.executed = true;
            canceled = true;
            #[allow(clippy::format_push_string)]
            response.push_str(&format!(
                "Canceled enforcement ID {} for {}\n",
                id, user.name
            ));

            // Notify the enforcement task that this enforcement has been canceled
            if let Some(tx) = &*ctx.data().enforcement_tx {
                let _ = tx
                    .send(EnforcementCheckRequest::CheckEnforcement {
                        enforcement_id: id.clone(),
                    })
                    .await;
            }
        }
    }

    if !canceled {
        response = format!("No pending enforcements found for {}", user.name);
    }

    // Save data
    if canceled {
        if let Err(e) = ctx.data().save().await {
            error!("Failed to save data after canceling warning: {}", e);
        }
    }

    ctx.say(response).await?;
    Ok(())
}

// // Admin check function for commands that require admin permissions
// async fn admin_check(ctx: Context<'_, Data, Error>) -> Result<bool, Error> {
//     // let guild = match ctx
//     //     .guild() {
//     //     Some(guild) => guild,
//     //     None => {
//     //         ctx.say("This command can only be used in a server").await?;
//     //         return Ok(false);
//     //     }
//     // }.clone();

//     if let Some(member) = ctx.author_member().await {
//         #[allow(deprecated)]
//         //let permissions = guild.member_permissions(&member);
//         let permissions = member.permissions(ctx)?;
//         return Ok(permissions.administrator() || permissions.manage_guild());
//     }
//     ctx.say("This command can only be used by administrators")
//         .await?;
//     Ok(false)
// }

#[cfg(test)]
mod tests {
    use super::*;

    // Test that the ping command is properly defined
    #[test]
    fn test_ping_command_definition() {
        let cmd = ping();
        assert_eq!(cmd.name, "ping");
        assert!(
            cmd.description
                .unwrap_or_else(Default::default)
                .contains("check if the bot is responsive")
        );
        assert!(cmd.guild_only);
    }

    // This test verifies that the ping command can be executed
    #[test]
    fn test_ping_command_can_be_called() {
        // This test just verifies that the ping command exists and can be called
        // We don't actually execute it since that would require a real Discord context
        let cmd = ping();
        assert!(cmd.create_as_slash_command().is_some());
    }
}
