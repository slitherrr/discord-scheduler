use crate::message_shim::MessageShim;

use chrono::{Datelike, Duration, Local, NaiveDate, Weekday};
use chronoutil::DateRule;
use itertools::Itertools;
use log::{error, info};
use serde::{Deserialize, Serialize};
use serenity::builder::{CreateActionRow, CreateButton, CreateComponents, CreateSelectMenu};
use serenity::client::Context;
use serenity::model::channel::Message;
use serenity::model::id::{RoleId, UserId};
use serenity::model::interactions::message_component::{ButtonStyle, MessageComponentInteraction};
use serenity::model::interactions::InteractionResponseType;
use std::collections::{HashMap, HashSet};
use std::sync::RwLock;
use std::time::Instant;

// Ephemeral messages can only be edited for a limited time after they are initally created;
// testing indicates that this limit is 15 minutes
const RESP_TIMEOUT: std::time::Duration = std::time::Duration::new(60 * 14, 0);

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum ResponseType {
    Normal,
    Blackout,
}

#[derive(Clone, Default, Serialize, Deserialize)]
pub struct Response {
    dates: HashSet<NaiveDate>,
}

impl From<HashSet<NaiveDate>> for Response {
    fn from(dates: HashSet<NaiveDate>) -> Self {
        Response { dates }
    }
}

#[derive(Serialize, Deserialize)]
pub struct Scheduler {
    owner: UserId,
    title: String,
    dates: Vec<NaiveDate>,
    #[serde(default)]
    blackout_dates: RwLock<HashSet<NaiveDate>>,
    group: Option<RoleId>,
    message: MessageShim,
    responses: RwLock<HashMap<UserId, Response>>,
    closed: bool,
}

impl Scheduler {
    pub fn new(
        owner: UserId,
        group: Option<RoleId>,
        message: Message,
        weeks: i64,
        skip: Option<i64>,
        title: &str,
        days: HashSet<Weekday>,
    ) -> Self {
        let today = Local::today().naive_local();
        let mut start_date = today.succ();
        while start_date.weekday() != Weekday::Sat {
            start_date = start_date.succ();
        }
        if let Some(skip) = skip {
            start_date += Duration::weeks(skip);
        }
        let end_date = start_date + Duration::weeks(weeks);
        let window = DateRule::daily(start_date).with_end(end_date);
        let dates = window.filter(|day| days.contains(&day.weekday())).collect();
        Self {
            owner,
            title: title.to_string(),
            dates,
            blackout_dates: Default::default(),
            group,
            message: message.into(),
            responses: Default::default(),
            closed: false,
        }
    }

    fn save(&self) {
        crate::write_file(&self.message.message_id, self);
    }

    pub async fn add_response(&self, ctx: &Context, user: UserId, response: Response) {
        self.responses.write().unwrap().insert(user, response);
        self.save();
        self.update_message(ctx).await;
    }

    pub async fn set_blackout(&self, ctx: &Context, response: Response) {
        *self.blackout_dates.write().unwrap() = response.dates;
        self.save();
        self.update_message(ctx).await;
    }

    fn get_responses(&self) -> String {
        let responses = self.responses.read().unwrap();
        if responses.is_empty() {
            "**0**".to_owned()
        } else {
            format!(
                "**{}** ({})",
                responses.len(),
                responses
                    .iter()
                    .map(|(id, _response)| format!("<@{}>", id))
                    .collect::<Vec<String>>()
                    .join(", ")
            )
        }
    }

    fn get_results(&self, detailed: bool) -> impl Iterator<Item = String> + '_ {
        let responses = self.responses.read().unwrap();
        let blackout_dates = self.blackout_dates.read().unwrap();
        let results: Vec<_> = self
            .dates
            .iter()
            .filter_map(|date| {
                if blackout_dates.contains(date) {
                    None
                } else {
                    let mut users = HashSet::new();
                    for (user_id, response) in responses.iter() {
                        if response.dates.contains(date) {
                            users.insert(user_id);
                        }
                    }
                    Some((date, users))
                }
            })
            .collect();
        let max = results
            .iter()
            .map(|(_, users)| users.len())
            .max()
            .unwrap_or(0);
        results
            .iter()
            .map(move |(date, users)| {
                let count = users.len();
                let date = date.format("%a %Y-%m-%d");
                let mut line = if max > 0 && count == max {
                    format!("__`{}:`__ {}", date, count)
                } else {
                    format!("`{}:` {}", date, count)
                };
                if detailed && !users.is_empty() {
                    line = format!(
                        "{} - {}",
                        line,
                        users
                            .iter()
                            .sorted()
                            .map(|uid| format!("<@{}>", uid))
                            .join(", ")
                    );
                }
                line
            })
            .collect_vec()
            .into_iter()
    }

    pub async fn update_message(&self, ctx: &Context) {
        let title = &self.title;
        let responses = self.get_responses();
        let results = self.get_results(false).join("\n");
        let closed = self.closed;
        let content = match &self.group {
            Some(role) => format!("<@&{}>", role),
            None => "".to_owned(),
        };
        self.message
            .edit(ctx, |m| {
                let mut ar = CreateActionRow::default();
                let mut text = "";
                if !closed {
                    ar.create_button(|b| b.label("Add response").custom_id("response"));
                    ar.create_button(|b| {
                        b.style(ButtonStyle::Secondary)
                            .label("Show details")
                            .custom_id("details")
                    });
                } else {
                    ar.create_button(|b| {
                        b.style(ButtonStyle::Secondary)
                            .label("Show details")
                            .custom_id("details")
                    });
                    text = "Final results";
                }
                m.content(content)
                    .embed(|e| {
                        e.title(title)
                            .description(text)
                            .field("Responded", responses, false)
                            .field("Results", &results, true)
                    })
                    .components(|c| c.add_action_row(ar))
                    .allowed_mentions(|am| am.roles(self.group))
                    .suppress_embeds(false)
            })
            .await
            .map_err(|e| error!("Cannot edit message: {}", e))
            .ok();
    }

    pub async fn show_details(&self, ctx: &Context, component: &MessageComponentInteraction) {
        component.defer(ctx).await.unwrap();
        let results = self.get_results(true);
        let mut messages: Vec<String> = vec![];
        let mut content = String::new();
        for line in results {
            assert!(line.len() < 2000);
            if content.len() + line.len() >= 2000 {
                messages.push(content);
                content = String::new()
            }
            content += &line;
            content.push('\n');
        }
        let last_content = content;
        for content in messages {
            component
                .create_followup_message(ctx, |m| m.ephemeral(true).content(content))
                .await
                .expect("Cannot send message");
        }
        component
            .create_followup_message(ctx, |m| {
                if component.user.id == self.owner {
                    let mut ar = CreateActionRow::default();
                    ar.create_button(|b| b.label("Add blackout dates").custom_id("blackout"));
                    m.components(|c| c.add_action_row(ar));
                }
                m.ephemeral(true).content(last_content)
            })
            .await
            .expect("Cannot send message");
    }

    pub async fn get_response(
        &self,
        ctx: &Context,
        component: &MessageComponentInteraction,
        resp_type: ResponseType,
    ) {
        let user = &component.user;

        if let Some(role) = self.group {
            let guild = component.guild_id.expect("Cannot get guild");
            let allowed = user
                .has_role(&ctx, guild, role)
                .await
                .expect("Cannot check role");
            if !allowed {
                component
                    .create_interaction_response(&ctx, |r| {
                        r.kind(InteractionResponseType::ChannelMessageWithSource)
                            .interaction_response_data(|m| {
                                m.content(format!("Only <@&{}> may respond", role))
                                    .ephemeral(true)
                            })
                    })
                    .await
                    .expect("Cannot send response");
                return;
            }
        };

        let mut response = match resp_type {
            ResponseType::Normal => self
                .responses
                .read()
                .unwrap()
                .get(&user.id)
                .cloned()
                .unwrap_or_default(),
            ResponseType::Blackout => self.blackout_dates.read().unwrap().clone().into(),
        };
        component
            .create_interaction_response(ctx, |r| {
                r.kind(InteractionResponseType::ChannelMessageWithSource)
                    .interaction_response_data(|m| {
                        m.ephemeral(true)
                            .components(|c| self.create_dm_buttons(&response, c, resp_type))
                    })
            })
            .await
            .expect("Cannot send DM");

        let expiration = Instant::now() + RESP_TIMEOUT;

        let message = component
            .get_interaction_response(ctx)
            .await
            .expect("Cannot get response message");
        loop {
            let interaction = message
                .await_component_interaction(ctx)
                .timeout(expiration - Instant::now())
                .await;
            let interaction = match interaction {
                Some(i) => i,
                None => {
                    info!("Response timed out");
                    component
                        .edit_original_interaction_response(ctx, |m| {
                            m.content("Response timed out").components(|c| c)
                        })
                        .await
                        .expect("Cannot update message");
                    return;
                }
            };
            interaction
                .defer(ctx)
                .await
                .expect("Cannot respond to button");
            let interaction_id = interaction.data.custom_id.as_str();
            match interaction_id {
                "submit" => {
                    if matches!(
                        component
                            .edit_original_interaction_response(ctx, |m| {
                                m.content("Response submitted").components(|c| c)
                            })
                            .await,
                        Err(_)
                    ) {
                        error!("Cannot update message");
                    }
                    break;
                }
                "select_all" => {
                    let blackout_dates = self.blackout_dates.read().unwrap();
                    response.dates = self
                        .dates
                        .iter()
                        .filter(|d| !blackout_dates.contains(d))
                        .cloned()
                        .collect()
                }
                "clear_all" => response.dates.clear(),
                "select" => {
                    let selections: Vec<usize> = interaction
                        .data
                        .values
                        .iter()
                        .map(|v| v.parse().unwrap())
                        .collect();
                    response.dates.clear();
                    for index in selections.iter() {
                        let date = &self.dates[*index];
                        let resp_dates = &mut response.dates;
                        resp_dates.insert(*date);
                    }
                }
                _ => panic!("Unexpected button: {interaction_id}"),
            }
            component
                .edit_original_interaction_response(ctx, |m| {
                    m.components(|c| self.create_dm_buttons(&response, c, resp_type))
                })
                .await
                .expect("Cannot update message");
        }

        match resp_type {
            ResponseType::Normal => self.add_response(ctx, user.id, response).await,
            ResponseType::Blackout => self.set_blackout(ctx, response).await,
        };
    }

    fn create_dm_buttons<'a>(
        &self,
        response: &Response,
        components: &'a mut CreateComponents,
        resp_type: ResponseType,
    ) -> &'a mut CreateComponents {
        let mut ar = CreateActionRow::default();
        let mut menu = CreateSelectMenu::default();
        let mut count = 0;
        menu.options(|m| {
            for (i, date) in self.dates.iter().enumerate() {
                if resp_type == ResponseType::Normal
                    && self.blackout_dates.read().unwrap().contains(date)
                {
                    continue;
                }
                m.create_option(|opt| {
                    count += 1;
                    opt.label(date.format("%a %b %d"));
                    opt.value(format!("{}", i));
                    opt.default_selection(response.dates.contains(date));
                    opt
                });
            }
            m
        });
        menu.custom_id("select");
        menu.min_values(0);
        menu.max_values(count);
        ar.add_select_menu(menu);
        components.add_action_row(ar);

        ar = CreateActionRow::default();

        if resp_type != ResponseType::Blackout {
            let mut button = CreateButton::default();
            button.label("Select all");
            button.custom_id("select_all");
            button.style(ButtonStyle::Success);
            ar.add_button(button);

            let mut button = CreateButton::default();
            button.label("Clear all");
            button.custom_id("clear_all");
            button.style(ButtonStyle::Secondary);
            ar.add_button(button);
        }

        let mut button = CreateButton::default();
        button.label("Submit");
        button.custom_id("submit");
        ar.add_button(button);

        components.add_action_row(ar)
    }
}
