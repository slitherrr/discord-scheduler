mod message_shim;
mod scheduler;
use crate::scheduler::{ResponseType, Scheduler};

use chrono::Weekday;
use clap::Parser;
use dotenv::dotenv;
use log::{error, info};
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
use std::panic;
use std::path::{Path, PathBuf};
use std::str::FromStr;
use tokio::sync::{RwLock, RwLockMappedWriteGuard, RwLockReadGuard, RwLockWriteGuard};

const DATA_DIR: &str = "data";
const MAX_WEEKS: usize = 10;

// All mutable accesses to Handler.schedulers go through this wrapper, which dumps the data to disk
// when it is dropped.
struct ScheduleWrapper<'a> {
    message_id: MessageId,
    scheduler: RwLockMappedWriteGuard<'a, Scheduler>,
}

impl<'a> Deref for ScheduleWrapper<'a> {
    type Target = Scheduler;

    fn deref(&self) -> &Self::Target {
        &*self.scheduler
    }
}

impl<'a> DerefMut for ScheduleWrapper<'a> {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut *self.scheduler
    }
}

impl<'a> Drop for ScheduleWrapper<'a> {
    fn drop(&mut self) {
        write_file(&self.message_id, self);
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

fn read_file(path: &Path) -> Option<(u64, Scheduler)> {
    let extension = path.extension().and_then(|e| e.to_str());
    if !matches!(extension, Some("json")) {
        return None;
    }
    let id: u64 = path
        .file_stem()
        .unwrap()
        .to_str()
        .unwrap()
        .parse()
        .expect("Cannot parse file name");
    let file = File::open(path).expect("Cannot open file");
    Some((
        id,
        serde_json::from_reader(file).expect("Cannot parse data"),
    ))
}

fn file_path(id: &MessageId) -> PathBuf {
    let mut path: PathBuf = DATA_DIR.into();
    path.push(id.as_u64().to_string());
    path.set_extension("json");
    path
}

fn write_file(id: &MessageId, scheduler: &Scheduler) {
    let file = File::create(file_path(id)).expect("Cannot create file");
    serde_json::to_writer(file, &scheduler).expect("Cannot serialize data");
}

fn delete_file(id: &MessageId) {
    std::fs::remove_file(file_path(id)).expect("Cannot delete file");
}

impl Handler {
    fn new(refresh: bool) -> Self {
        let data_dir = std::fs::metadata(DATA_DIR);
        let is_dir = match data_dir {
            Ok(f) => f.is_dir(),
            Err(_) => false,
        };
        if !is_dir {
            std::fs::create_dir(DATA_DIR).expect("Cannot create data dir");
        }

        let mut schedulers: HashMap<MessageId, Scheduler> = HashMap::default();
        for f in std::fs::read_dir(DATA_DIR).expect("Cannot read data dir") {
            let path = f.unwrap().path();
            if let Some((id, s)) = read_file(&path) {
                schedulers.insert(id.into(), s);
            }
        }
        info!("{} schedulers loaded", schedulers.len());

        Handler {
            refresh,
            schedulers: RwLock::new(schedulers),
        }
    }

    async fn get_scheduler(
        &self,
        message_id: &MessageId,
    ) -> Option<RwLockReadGuard<'_, Scheduler>> {
        let schedulers = self.schedulers.read().await;
        RwLockReadGuard::try_map(schedulers, |s| s.get(message_id)).ok()
    }

    async fn get_mut_scheduler(&self, message_id: MessageId) -> Option<ScheduleWrapper<'_>> {
        let schedulers = self.schedulers.write().await;
        let scheduler = RwLockWriteGuard::try_map(schedulers, |s| s.get_mut(&message_id)).ok()?;
        Some(ScheduleWrapper {
            message_id,
            scheduler,
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
        let scheduler = Scheduler::new(command.user.id, group, message, weeks, skip, title, days);
        scheduler.update_message(&ctx).await;
        let mut schedulers = self.schedulers.write().await;
        write_file(&message_id, &scheduler);
        schedulers.insert(message_id, scheduler);
    }

    async fn handle_get_response(
        &self,
        ctx: Context,
        component: &MessageComponentInteraction,
        resp_type: ResponseType,
    ) {
        let message_id = match resp_type {
            ResponseType::Normal => component.message.id,
            ResponseType::Blackout | ResponseType::CreateEvent => component
                .message
                .message_reference
                .as_ref()
                .expect("Cannot find message for DM")
                .message_id
                .unwrap(),
        };
        let scheduler = self
            .get_scheduler(&message_id)
            .await
            .expect("Cannot find scheduler");
        if !scheduler.can_respond(&ctx, component).await {
            return;
        }
        let dates = scheduler.get_dates();
        let blackout_dates = scheduler.get_blackout_dates();
        let event_dates = scheduler.get_event_dates();
        let response = match resp_type {
            ResponseType::Normal => scheduler
                .get_user_response(&component.user.id)
                .unwrap_or_default(),
            ResponseType::Blackout => blackout_dates.clone().into(),
            ResponseType::CreateEvent => event_dates.clone().into(),
        };
        drop(scheduler); // Release the lock so we don't block other interactions
        // TODO: Actually make the event in here somewhere
        if let Some(response) =
            scheduler::get_response(&ctx, component, response, dates, blackout_dates, event_dates, resp_type)
                .await
        {
            let mut scheduler = self.get_mut_scheduler(message_id).await.unwrap();
            match resp_type {
                ResponseType::Normal => {
                    scheduler
                        .add_response(&ctx, component.user.id, response)
                        .await
                }
                ResponseType::Blackout => scheduler.set_blackout(&ctx, response).await,
                ResponseType::CreateEvent => scheduler.set_events_and_update_to_match(&ctx, response).await,
            }
        }
    }

    async fn handle_show_details(&self, ctx: Context, component: &MessageComponentInteraction) {
        let message_id = component.message.id;
        let scheduler = self
            .get_scheduler(&message_id)
            .await
            .expect("Cannot find scheduler");
        scheduler.show_details(&ctx, component).await;
    }

    async fn handle_close(&self, ctx: Context, component: &MessageComponentInteraction) {
        let scheduler = self
            .get_scheduler(&component.message.id)
            .await
            .expect("Cannot find scheduler");
        scheduler.close_prompt(&ctx, component).await;
    }

    async fn handle_close_yes(&self, ctx: Context, component: &MessageComponentInteraction) {
        component
            .defer(&ctx)
            .await
            .expect("Cannot respond to button");
        let message_ref = component.message.message_reference.as_ref().unwrap();
        let mut scheduler = self
            .get_mut_scheduler(message_ref.message_id.unwrap())
            .await
            .expect("Cannot find scheduler");
        scheduler.handle_close(&ctx, component).await;
    }
}

#[async_trait]
impl EventHandler for Handler {
    async fn interaction_create(&self, ctx: Context, interaction: Interaction) {
        match interaction {
            Interaction::ApplicationCommand(command) => {
                let user = command.user.name.as_str();
                let command_name = command.data.name.as_str();
                info!("{} <{}>", command_name, user);
                match command_name {
                    "schedule" => self.create_scheduler(ctx, command).await,
                    _ => panic!("Unexpected command: {}", command_name),
                }
            }
            Interaction::MessageComponent(component) => {
                let user = component.user.name.as_str();
                let button_id = component.data.custom_id.as_str();
                info!("{} <{}>", button_id, user);
                match button_id {
                    "response" => {
                        self.handle_get_response(ctx, &component, ResponseType::Normal)
                            .await
                    }
                    "blackout" => {
                        self.handle_get_response(ctx, &component, ResponseType::Blackout)
                            .await
                    }
                    "create_event" => {
                        self.handle_get_response(ctx, &component, ResponseType::CreateEvent)
                            .await
                    }
                    "details" => self.handle_show_details(ctx, &component).await,
                    "close" => self.handle_close(ctx, &component).await,
                    "close_yes" => self.handle_close_yes(ctx, &component).await,
                    _ => (),
                }
            }
            _ => panic!("Unexpected interaction: {:?}", interaction),
        }
    }

    async fn ready(&self, ctx: Context, _ready: Ready) {
        info!("ready");

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
            for (_, scheduler) in self.schedulers.read().await.iter() {
                scheduler.update_message(&ctx).await;
            }
        }
    }

    async fn message_delete(
        &self,
        _ctx: Context,
        _channel_id: ChannelId,
        deleted_message_id: MessageId,
        _guild_id: Option<GuildId>,
    ) {
        let mut schedulers = self.schedulers.write().await;
        if let Some(_scheduler) = schedulers.remove(&deleted_message_id) {
            info!("scheduler message deleted: {}", deleted_message_id);
            delete_file(&deleted_message_id);
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
    env_logger::Builder::new()
        .target(env_logger::Target::Stdout)
        .filter(Some("scheduler"), log::LevelFilter::Info)
        .init();
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

    panic::set_hook(Box::new(move |p| {
        error!("{}", p);
    }));

    // Finally, start a single shard, and start listening to events.
    // Shards will automatically attempt to reconnect, and will perform
    // exponential backoff until it reconnects.
    if let Err(why) = client.start().await {
        error!("Client error: {:?}", why);
    }
}
