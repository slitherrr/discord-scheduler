mod message_shim;
mod scheduler;
use crate::scheduler::Scheduler;

use chrono::Weekday;
use clap::Parser;
use dotenv::dotenv;
use serenity::async_trait;
use serenity::client::{Context, EventHandler};
use serenity::json::Value;
use serenity::model::gateway::Ready;
use serenity::model::id::{ChannelId, GuildId, MessageId, RoleId};
use serenity::model::interactions::application_command::{
    ApplicationCommand, ApplicationCommandInteraction, ApplicationCommandOptionType,
};
use serenity::model::interactions::message_component::MessageComponentInteraction;
use serenity::model::interactions::{Interaction, InteractionResponseType};
use serenity::prelude::*;
use std::collections::{HashMap, HashSet};
use std::env;
use std::fs::File;
use std::ops::{Deref, DerefMut};
use std::str::FromStr;
use tokio::sync::{RwLock, RwLockWriteGuard};

const MAX_WEEKS: usize = 10;

// All mutable accesses to Handler.schedulers go through this wrapper, which dumps the data to disk
// when it is dropped.
struct ScheduleWrapper<'a> {
    handler: &'a Handler,
    message_id: MessageId,
    schedulers: RwLockWriteGuard<'a, HashMap<MessageId, Scheduler>>,
}

impl<'a> Deref for ScheduleWrapper<'a> {
    type Target = Scheduler;

    fn deref(&self) -> &Self::Target {
        self.schedulers
            .get(&self.message_id)
            .expect("Cannot find scheduler")
    }
}

impl<'a> DerefMut for ScheduleWrapper<'a> {
    fn deref_mut(&mut self) -> &mut Self::Target {
        self.schedulers
            .get_mut(&self.message_id)
            .expect("Cannot find scheduler")
    }
}

impl<'a> Drop for ScheduleWrapper<'a> {
    fn drop(&mut self) {
        self.handler.dump(&*self.schedulers);
    }
}

#[derive(Default)]
struct Handler {
    refresh: bool,
    schedulers: RwLock<HashMap<MessageId, Scheduler>>,
}

async fn send_error(ctx: &Context, command: &ApplicationCommandInteraction, msg: &str) {
    command
        .create_interaction_response(ctx, |c| {
            c.kind(InteractionResponseType::ChannelMessageWithSource)
                .interaction_response_data(|m| m.content(msg).ephemeral(true))
        })
        .await
        .expect("Cannot send error response");
}

impl Handler {
    fn new(refresh: bool) -> Self {
        let schedulers: HashMap<MessageId, Scheduler> = File::open("data.json")
            .map(|f| {
                println!("Loading existing data");
                serde_json::from_reader(f).expect("Cannot parse data")
            })
            .unwrap_or_default();
        Handler {
            refresh,
            schedulers: RwLock::new(schedulers),
        }
    }

    fn dump(&self, schedulers: &HashMap<MessageId, Scheduler>) {
        let file = File::create("data.json").expect("Cannot create file");
        serde_json::to_writer(file, &schedulers).expect("Cannot serialize data");
    }

    async fn get_scheduler(&self, message_id: MessageId) -> Option<ScheduleWrapper<'_>> {
        let schedulers = self.schedulers.write().await;
        schedulers.get(&message_id)?;
        Some(ScheduleWrapper {
            handler: self,
            message_id,
            schedulers,
        })
    }

    async fn create_scheduler(&self, ctx: Context, command: ApplicationCommandInteraction) {
        let options: HashMap<&str, &Value> = command
            .data
            .options
            .iter()
            .filter_map(|o| o.value.as_ref().map(|v| (o.name.as_ref(), v)))
            .collect();
        let title = options
            .get("description")
            .expect("Cannot find description option");
        let title = title.as_str().expect("Caption has incorrect type");
        if title.len() > 256 {
            send_error(&ctx, &command, "Description is too long").await;
            return;
        }
        let group = options.get("group").map(|v| {
            RoleId::from_str(v.as_str().expect("Group has incorrect type"))
                .expect("Error parsing role")
        });
        let weeks = match options.get("weeks") {
            Some(weeks) => weeks.as_i64().expect("Weeks has incorrect type"),
            None => MAX_WEEKS as i64,
        };
        let days = options
            .get("days")
            .map(|s| {
                s.as_str()
                    .expect("Days has incorrect type")
                    .split('+')
                    .map(|d| Weekday::from_str(d).expect("Cannot parse day"))
                    .collect::<HashSet<Weekday>>()
            })
            .unwrap_or_else(|| HashSet::from([Weekday::Sat, Weekday::Sun]));
        let skip = options
            .get("skip")
            .map(|v| v.as_i64().expect("Skip has incorrect type"));
        command
            .create_interaction_response(&ctx.http, |response| {
                response
                    .kind(InteractionResponseType::ChannelMessageWithSource)
                    .interaction_response_data(|m| m.content("Please wait..."))
            })
            .await
            .expect("Cannot respond to slash command");
        let message = command
            .get_interaction_response(&ctx)
            .await
            .expect("Cannot get message");
        let message_id = message.id;
        let mut scheduler =
            Scheduler::new(command.user.id, group, message, weeks, skip, title, days);
        scheduler.update_message(&ctx).await;
        let mut schedulers = self.schedulers.write().await;
        schedulers.insert(message_id, scheduler);
        self.dump(&schedulers);
    }

    async fn handle_add_response(&self, ctx: Context, component: MessageComponentInteraction) {
        let message_id = component.message.id;
        let mut scheduler = self
            .get_scheduler(message_id)
            .await
            .expect("Cannot find scheduler");
        scheduler.send_dm(&ctx, &component).await;
    }

    async fn handle_show_details(&self, ctx: Context, component: &MessageComponentInteraction) {
        let message_id = component.message.id;
        let schedulers = self.schedulers.read().await;
        let scheduler = schedulers.get(&message_id).expect("Cannot find scheduler");
        scheduler.show_details(&ctx, component).await;
    }

    async fn handle_dm_submit(&self, ctx: Context, component: MessageComponentInteraction) {
        component
            .defer(&ctx)
            .await
            .expect("Cannot respond to dm button");
        let scheduler_message = component
            .message
            .message_reference
            .as_ref()
            .expect("Cannot find message for DM")
            .message_id
            .unwrap();
        let mut scheduler = self
            .get_scheduler(scheduler_message)
            .await
            .expect("Cannot find scheduler");
        scheduler.handle_response(&ctx, component).await;
    }

    async fn handle_close(&self, ctx: Context, component: MessageComponentInteraction) {
        let mut scheduler = self
            .get_scheduler(component.message.id)
            .await
            .expect("Cannot find scheduler");
        scheduler.close_prompt(&ctx, component).await;
    }

    async fn handle_close_yes(&self, ctx: Context, component: MessageComponentInteraction) {
        component
            .defer(&ctx)
            .await
            .expect("Cannot respond to button");
        let message_ref = component.message.message_reference.as_ref().unwrap();
        let mut scheduler = self
            .get_scheduler(message_ref.message_id.unwrap())
            .await
            .expect("Cannot find scheduler");
        scheduler.handle_close(&ctx, component).await;
    }

    async fn handle_dm_select(
        &self,
        ctx: Context,
        component: &MessageComponentInteraction,
        data: &str,
    ) {
        component
            .defer(&ctx)
            .await
            .expect("Cannot respond to dm button");
        let scheduler_message = component
            .message
            .message_reference
            .as_ref()
            .expect("Cannot find message for DM")
            .message_id
            .unwrap();
        let mut scheduler = self
            .get_scheduler(scheduler_message)
            .await
            .expect("Cannot find scheduler");
        let index = data.parse().expect("Cannot parse index");
        scheduler.handle_select(&ctx, component, index).await;
    }

    async fn handle_dm_select_all(&self, ctx: Context, component: &MessageComponentInteraction) {
        component
            .defer(&ctx)
            .await
            .expect("Cannot respond to dm button");
        let scheduler_message = component
            .message
            .message_reference
            .as_ref()
            .expect("Cannot find message for DM")
            .message_id
            .unwrap();
        let mut scheduler = self
            .get_scheduler(scheduler_message)
            .await
            .expect("Cannot find scheduler");
        scheduler.handle_select_all(&ctx, component).await;
    }

    async fn handle_dm_clear_all(&self, ctx: Context, component: &MessageComponentInteraction) {
        component
            .defer(&ctx)
            .await
            .expect("Cannot respond to dm button");
        let scheduler_message = component
            .message
            .message_reference
            .as_ref()
            .expect("Cannot find message for DM")
            .message_id
            .unwrap();
        let mut scheduler = self
            .get_scheduler(scheduler_message)
            .await
            .expect("Cannot find scheduler");
        scheduler.handle_clear_all(&ctx, component).await;
    }
}

#[async_trait]
impl EventHandler for Handler {
    async fn interaction_create(&self, ctx: Context, interaction: Interaction) {
        match interaction {
            Interaction::ApplicationCommand(command) => {
                let user = command.user.name.as_str();
                let command_name = command.data.name.as_str();
                println!("{} <{}>", command_name, user);
                match command_name {
                    "schedule" => self.create_scheduler(ctx, command).await,
                    _ => panic!("Unexpected command: {}", command_name),
                }
            }
            Interaction::MessageComponent(component) => {
                let user = component.user.name.as_str();
                let button_id = component.data.custom_id.as_str();
                println!("{} <{}>", button_id, user);
                match button_id {
                    "response" => self.handle_add_response(ctx, component).await,
                    "details" => self.handle_show_details(ctx, &component).await,
                    "dm_submit" => self.handle_dm_submit(ctx, component).await,
                    "close" => self.handle_close(ctx, component).await,
                    "close_yes" => self.handle_close_yes(ctx, component).await,
                    "dm_select_all" => self.handle_dm_select_all(ctx, &component).await,
                    "dm_clear_all" => self.handle_dm_clear_all(ctx, &component).await,
                    _ => {
                        if let Some((button_id, rest)) = button_id.split_once(' ') {
                            match button_id {
                                "select" => self.handle_dm_select(ctx, &component, rest).await,
                                _ => panic!("Unexpected button: {}", button_id),
                            }
                        } else {
                            panic!("Unexpected button: {}", button_id);
                        }
                    }
                }
            }
            _ => panic!("Unexpected interaction: {:?}", interaction),
        }
    }

    async fn ready(&self, ctx: Context, _ready: Ready) {
        println!("ready");

        ApplicationCommand::create_global_application_command(&ctx, |command| {
            command
                .name("schedule")
                .description("Create a scheduler")
                .create_option(|o| {
                    o.name("description")
                        .description("event description")
                        .kind(ApplicationCommandOptionType::String)
                        .required(true)
                })
                .create_option(|o| {
                    o.name("group")
                        .description("player group")
                        .kind(ApplicationCommandOptionType::Role)
                })
                .create_option(|o| {
                    o.name("weeks")
                        .description("number of weeks")
                        .kind(ApplicationCommandOptionType::Integer)
                        .min_int_value(1)
                        .max_int_value(MAX_WEEKS)
                })
                .create_option(|o| {
                    o.name("skip")
                        .description("weeks before start")
                        .kind(ApplicationCommandOptionType::Integer)
                        .min_int_value(0)
                })
                .create_option(|o| {
                    o.name("days")
                        .description("weekdays to include")
                        .kind(ApplicationCommandOptionType::String)
                        .add_string_choice("Saturday + Sunday", "Sat+Sun")
                        .add_string_choice("Saturday", "Sat")
                        .add_string_choice("Sunday", "Sun")
                })
        })
        .await
        .expect("Cannot create command");

        if self.refresh {
            for (_, scheduler) in self.schedulers.write().await.iter_mut() {
                scheduler.update_message(&ctx).await;
            }
        }
    }

    async fn message_delete(
        &self,
        ctx: Context,
        _channel_id: ChannelId,
        deleted_message_id: MessageId,
        _guild_id: Option<GuildId>,
    ) {
        let mut schedulers = self.schedulers.write().await;
        if let Some(mut scheduler) = schedulers.remove(&deleted_message_id) {
            println!("scheduler message deleted: {}", deleted_message_id);
            scheduler.close(&ctx).await;
            self.dump(&schedulers);
        }
    }
}

#[derive(Parser)]
#[clap(author, version, about, long_about = None)]
struct Cli {
    #[clap(long, action)]
    refresh: bool,
}

#[tokio::main]
async fn main() {
    let cli = Cli::parse();

    dotenv().ok();
    // Configure the client with your Discord bot token in the environment.
    let token = env::var("DISCORD_TOKEN").expect("Expected a token in the environment");

    // Build our client.
    let intents = GatewayIntents::GUILD_MESSAGES;
    let mut client = Client::builder(token, intents)
        .event_handler(Handler::new(cli.refresh))
        .await
        .expect("Error creating client");

    // Finally, start a single shard, and start listening to events.
    // Shards will automatically attempt to reconnect, and will perform
    // exponential backoff until it reconnects.
    if let Err(why) = client.start().await {
        println!("Client error: {:?}", why);
    }
}
