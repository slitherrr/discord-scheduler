use crate::message_shim::MessageShim;
use crate::MAX_WEEKS;

use chrono::{Datelike, Duration, Local, NaiveDate, Weekday};
use chronoutil::DateRule;
use itertools::Itertools;
use serde::{Deserialize, Serialize};
use serenity::builder::{CreateActionRow, CreateButton, CreateComponents};
use serenity::client::Context;
use serenity::model::channel::Message;
use serenity::model::id::{RoleId, UserId};
use serenity::model::interactions::message_component::{ButtonStyle, MessageComponentInteraction};
use serenity::model::interactions::InteractionResponseType;
use std::collections::{HashMap, HashSet};

#[derive(Clone, Default, Serialize, Deserialize)]
struct Response {
    dates: HashSet<NaiveDate>,
}

#[derive(Serialize, Deserialize)]
pub struct Scheduler {
    owner: UserId,
    title: String,
    dates: Vec<NaiveDate>,
    group: Option<RoleId>,
    message: MessageShim,
    pending_responses: HashMap<UserId, Response>,
    responses: HashMap<UserId, Response>,
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
            group,
            message: message.into(),
            pending_responses: Default::default(),
            responses: Default::default(),
            closed: false,
        }
    }

    fn get_responses(&self) -> String {
        if self.responses.is_empty() {
            "**0**".to_owned()
        } else {
            format!(
                "**{}** ({})",
                self.responses.len(),
                self.responses
                    .iter()
                    .map(|(id, _response)| format!("<@{}>", id))
                    .collect::<Vec<String>>()
                    .join(", ")
            )
        }
    }

    fn get_results(&self, detailed: bool) -> impl Iterator<Item = String> + '_ {
        let results = self.dates.iter().map(|date| {
            let mut users = HashSet::new();
            for (user_id, response) in self.responses.iter() {
                if response.dates.contains(date) {
                    users.insert(user_id);
                }
            }
            (date, users)
        });
        let max = results
            .clone()
            .map(|(_, users)| users.len())
            .max()
            .unwrap_or(0);
        results.map(move |(date, users)| {
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
                    //ar.create_button(|b|
                    //    b
                    //        .style(ButtonStyle::Danger)
                    //        .label("Close")
                    //        .custom_id("close")
                    //);
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
            })
            .await
            .map_err(|e| println!("Cannot edit message: {}", e))
            .ok();
    }

    pub async fn send_dm(&mut self, ctx: &Context, component: &MessageComponentInteraction) {
        let user = &component.user;
        let guild = component.guild_id.expect("Cannot get guild");
        if let Some(role) = self.group {
            if !user
                .has_role(ctx, guild, role)
                .await
                .expect("Cannot check role")
            {
                component
                    .create_interaction_response(ctx, |r| {
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
        }
        self.pending_responses.remove(&user.id);
        let response = self.responses.get(&user.id).cloned().unwrap_or_default();
        component
            .create_interaction_response(ctx, |r| {
                r.kind(InteractionResponseType::ChannelMessageWithSource)
                    .interaction_response_data(|m| {
                        m.ephemeral(true)
                            .components(|c| create_dm_buttons(&self.dates, &response, c))
                    })
            })
            .await
            .expect("Cannot send DM");
        self.pending_responses.insert(user.id, response);
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
        messages.push(content);
        for content in messages {
            component
                .create_followup_message(ctx, |m| m.ephemeral(true).content(content))
                .await
                .expect("Cannot send message");
        }
    }

    async fn update_dm(&self, ctx: &Context, component: &MessageComponentInteraction) {
        let response = self
            .pending_responses
            .get(&component.user.id)
            .expect("Cannot find response for user");
        component
            .edit_original_interaction_response(ctx, |m| {
                m.components(|c| create_dm_buttons(&self.dates, response, c))
            })
            .await
            .expect("Cannot update DM");
    }

    pub async fn handle_select(
        &mut self,
        ctx: &Context,
        component: &MessageComponentInteraction,
        index: usize,
    ) {
        let response = self
            .pending_responses
            .get_mut(&component.user.id)
            .expect("Cannot find response for user");
        let date = &self.dates[index];
        let dates = &mut response.dates;
        if dates.contains(date) {
            dates.remove(date);
        } else {
            dates.insert(*date);
        }
        self.update_dm(ctx, component).await;
    }

    pub async fn handle_select_all(
        &mut self,
        ctx: &Context,
        component: &MessageComponentInteraction,
    ) {
        let response = self
            .pending_responses
            .get_mut(&component.user.id)
            .expect("Cannot find response for user");
        response.dates = self.dates.iter().cloned().collect();
        self.update_dm(ctx, component).await;
    }

    pub async fn handle_clear_all(
        &mut self,
        ctx: &Context,
        component: &MessageComponentInteraction,
    ) {
        let response = self
            .pending_responses
            .get_mut(&component.user.id)
            .expect("Cannot find response for user");
        response.dates.clear();
        self.update_dm(ctx, component).await;
    }

    pub async fn handle_response(&mut self, ctx: &Context, component: MessageComponentInteraction) {
        let response = self
            .pending_responses
            .remove(&component.user.id)
            .expect("Cannot find pending response for user");
        self.responses.insert(component.user.id, response);
        self.update_message(ctx).await;
        component
            .edit_original_interaction_response(ctx, |m| {
                m.content("Response submitted").components(|c| c)
            })
            .await
            .unwrap();
    }

    pub async fn close_prompt(&self, ctx: &Context, component: MessageComponentInteraction) {
        if component.user.id != self.owner {
            component
                .create_interaction_response(ctx, |response| {
                    response
                        .kind(InteractionResponseType::ChannelMessageWithSource)
                        .interaction_response_data(|m| {
                            m.ephemeral(true).content("Only owner can close")
                        })
                })
                .await
                .expect("Cannot send message");
            return;
        }

        component
            .create_interaction_response(ctx, |r| {
                r.kind(InteractionResponseType::ChannelMessageWithSource)
                    .interaction_response_data(|m| {
                        m.ephemeral(true).content("Finalize?").components(|c| {
                            c.create_action_row(|ar| {
                                ar.create_button(|b| b.label("Yes").custom_id("close_yes"))
                            })
                        })
                    })
            })
            .await
            .expect("Cannot send message");
    }

    pub async fn handle_close(&mut self, ctx: &Context, component: MessageComponentInteraction) {
        component
            .defer(ctx)
            .await
            .expect("Cannot respond to button");
        component
            .edit_original_interaction_response(ctx, |m| m.content("Closed!").components(|c| c))
            .await
            .expect("Cannot edit message");
        self.close(ctx).await;
    }

    pub async fn close(&mut self, ctx: &Context) {
        self.closed = true;
        self.update_message(ctx).await;
    }
}

fn create_dm_buttons<'a>(
    dates: &Vec<NaiveDate>,
    response: &Response,
    components: &'a mut CreateComponents,
) -> &'a mut CreateComponents {
    let count = dates.len();
    if count > 2 * MAX_WEEKS {
        panic!("Too many dates!");
    }
    let per_row = std::cmp::max(2, (count as f32 / 4f32).ceil() as usize);

    let mut ar = CreateActionRow::default();
    for (i, date) in dates.iter().enumerate() {
        if i > 0 && i % per_row == 0 {
            components.add_action_row(ar);
            ar = CreateActionRow::default();
        }
        let mut button = CreateButton::default();
        button.label(date.format("%a %b %d"));
        button.custom_id(format!("select {}", i));
        button.style(if response.dates.contains(date) {
            ButtonStyle::Success
        } else {
            ButtonStyle::Secondary
        });
        ar.add_button(button);
    }
    components.add_action_row(ar);

    ar = CreateActionRow::default();

    let mut button = CreateButton::default();
    button.label("Select all");
    button.custom_id("dm_select_all");
    button.style(ButtonStyle::Success);
    ar.add_button(button);

    let mut button = CreateButton::default();
    button.label("Clear all");
    button.custom_id("dm_clear_all");
    button.style(ButtonStyle::Secondary);
    ar.add_button(button);

    let mut button = CreateButton::default();
    button.label("Submit");
    button.custom_id("dm_submit");
    ar.add_button(button);

    components.add_action_row(ar)
}
