mod message_shim;
mod scheduler;
use crate::scheduler::{ResponseType, Scheduler};

use chrono::Weekday;
use clap::Parser;
use dotenv::dotenv;
use lockfree::map::Map;
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
use std::panic;
use std::path::{Path, PathBuf};
use std::str::FromStr;

const DATA_DIR: &str = "data";
const MAX_WEEKS: usize = 10;

#[derive(Default)]
struct Handler {
    refresh: bool,
    schedulers: Map<MessageId, Scheduler>,
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

        let schedulers: Map<MessageId, Scheduler> = Map::new();
        let mut count = 0;
        for f in std::fs::read_dir(DATA_DIR).expect("Cannot read data dir") {
            let path = f.unwrap().path();
            if let Some((id, s)) = read_file(&path) {
                schedulers.insert(id.into(), s);
                count += 1;
            }
        }
        info!("{} schedulers loaded", count);

        Handler {
            refresh,
            schedulers,
        }
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
        write_file(&message_id, &scheduler);
        self.schedulers.insert(message_id, scheduler);
    }

    async fn handle_get_response(
        &self,
        ctx: Context,
        component: &MessageComponentInteraction,
        resp_type: ResponseType,
    ) {
        let message_id = match resp_type {
            ResponseType::Normal => component.message.id,
            ResponseType::Blackout => component
                .message
                .message_reference
                .as_ref()
                .expect("Cannot find message for DM")
                .message_id
                .unwrap(),
        };
        let scheduler = self
            .schedulers
            .get(&message_id)
            .expect("Cannot find scheduler");
        scheduler
            .val()
            .get_response(&ctx, component, resp_type)
            .await
    }

    async fn handle_show_details(&self, ctx: Context, component: &MessageComponentInteraction) {
        let message_id = component.message.id;
        let scheduler = self
            .schedulers
            .get(&message_id)
            .expect("Cannot find scheduler");
        scheduler.val().show_details(&ctx, component).await;
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
                    "details" => self.handle_show_details(ctx, &component).await,
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
            for entry in self.schedulers.iter() {
                let scheduler = entry.val();
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
        if let Some(_scheduler) = self.schedulers.remove(&deleted_message_id) {
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
