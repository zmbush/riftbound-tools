use cached::{Cached as _, TimedCache};
use poise::serenity_prelude as serenity;
use std::{
    collections::{BTreeMap, HashMap},
    fmt::Write as _,
    io::Write,
    sync::Arc,
    time::Duration,
};
use tabwriter::TabWriter;
use tokio::sync::{Mutex, RwLock, RwLockMappedWriteGuard, RwLockReadGuard, RwLockWriteGuard};
use tracing::{error, info, warn};

use anyhow::Context as _;
use clap::Parser;
use once_cell::sync::Lazy;
use reqwest::{IntoUrl, Url};
use serde::{de::DeserializeOwned, Deserialize, Deserializer, Serialize};

const REGISTRY: &str = "registry.json";

static API_URL: Lazy<Url> = Lazy::new(|| {
    Url::parse("https://api.cloudflare.riftbound.uvsgames.com/hydraproxy/api/v2/")
        .expect("invalid api url")
});

static DECK_URL_BASE: Lazy<Url> =
    Lazy::new(|| API_URL.join("deckbuilder/decks/").expect("invalid path"));
fn deck_url(deck_id: &str) -> Result<Url, url::ParseError> {
    DECK_URL_BASE.join(deck_id)
}

static EVENTS_URL_BASE: Lazy<Url> = Lazy::new(|| API_URL.join("events/").expect("invalid path"));
fn event_url(event_id: u32) -> Url {
    EVENTS_URL_BASE
        .join(&format!("{event_id}/"))
        .expect("invalid path")
}
fn registrations(event_id: u32) -> Url {
    event_url(event_id)
        .join("registrations/")
        .expect("invalid path")
}

static ROUNDS_URL_BASE: Lazy<Url> =
    Lazy::new(|| API_URL.join("tournament-rounds/").expect("invalid path"));
fn tournament_rounds_url(event_id: u32) -> Url {
    ROUNDS_URL_BASE
        .join(&format!("{event_id}/"))
        .expect("invalid path")
}
fn matches_url(event_id: u32, page_size: usize, page: u32, player_name: Option<&str>) -> Url {
    let mut url = tournament_rounds_url(event_id)
        .join("matches/paginated/")
        .expect("invalid path");

    {
        let mut query = url.query_pairs_mut();
        query
            .append_pair("page_size", &page_size.to_string())
            .append_pair("page", &page.to_string());

        if let Some(name) = player_name {
            query.append_pair("player_name", name);
        }
    }

    url
}
fn standings_url(event_id: u32, page_size: usize, page: u32, player_name: Option<&str>) -> Url {
    let mut url = tournament_rounds_url(event_id)
        .join("standings/paginated/")
        .expect("invalid path");

    {
        let mut query = url.query_pairs_mut();
        query
            .append_pair("page_size", &page_size.to_string())
            .append_pair("page", &page.to_string());

        if let Some(name) = player_name {
            query.append_pair("player_name", name);
        }
    }

    url
}

#[derive(Debug, Deserialize)]
struct Player {
    id: u32,
    best_identifier: String,
}

#[derive(Debug, Deserialize)]
struct User {
    id: u32,
    pronouns: Option<String>,
    country_code: Option<String>,
}

#[derive(Debug, Deserialize)]
struct DeckDefiningCard {
    id: String,
    name: String,
    image_url: String,
}

#[derive(Debug, Deserialize)]
struct UserEventStatus {
    id: u32,
    matches_won: u32,
    matches_drawn: u32,
    matches_lost: u32,
    total_match_points: u32,
    best_identifier: String,
    user: User,
    deck_defining_card: Option<DeckDefiningCard>,
}

#[derive(Debug, Deserialize)]
struct StandingsResult {
    round_number: u32,
    id: u32,
    rank: usize,
    record: String,
    match_record: String,
    match_points: u32,
    opponent_match_win_percentage: f32,
    game_win_percentage: f32,
    opponent_game_win_percentage: f32,
    points: u32,
    player: Player,
    user_event_status: UserEventStatus,
}

#[derive(Debug, Deserialize)]
struct PaginationResult<Result> {
    next_page_number: Option<u32>,
    results: Vec<Result>,
}

trait Paginated {
    type Single: DeserializeOwned;

    fn page_url(id: u32, page_size: usize, page: u32) -> Url;
    fn construct(pages: Vec<Vec<Self::Single>>) -> Self;
}

impl Paginated for Standings {
    type Single = StandingsResult;

    fn page_url(id: u32, page_size: usize, page: u32) -> Url {
        standings_url(id, page_size, page, None)
    }

    fn construct(pages: Vec<Vec<StandingsResult>>) -> Standings {
        Standings {
            standings: pages
                .into_iter()
                .flat_map(|page| page.into_iter())
                .collect(),
        }
    }
}

#[derive(Default, Debug, Deserialize)]
struct Standings {
    standings: Vec<StandingsResult>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
enum MatchStatus {
    Complete,
    InProgress,
    #[serde(untagged)]
    Unknown(String),
}

#[derive(Debug, Deserialize)]
struct PlayerMatchRelationship {
    id: u32,
    player: Player,
    user_event_status: UserEventStatus,
}

#[derive(Debug, Deserialize)]
#[serde(untagged)]
enum MatchResultCompleted {
    /// A winning player! There is a result.
    Win {
        games_drawn: u32,
        games_won_by_winner: u32,
        games_won_by_loser: u32,
        winning_player: u32,
    },

    /// A result with no winning player means a tie.
    Tie {
        games_drawn: u32,
        games_won_by_winner: u32,
        games_won_by_loser: u32,
        match_is_intentional_draw: bool,
        match_is_unintentional_draw: bool,
    },

    Bye,
}

#[derive(Debug)]
enum MatchResult {
    Complete(MatchResultCompleted),
    InProgress,
}

impl<'de> Deserialize<'de> for MatchResult {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        #[derive(Deserialize)]
        struct MatchResultInner {
            match_is_bye: bool,

            #[serde(flatten)]
            complete: Option<MatchResultCompleted>,
        }
        let result = MatchResultInner::deserialize(deserializer)?;

        Ok(match result {
            MatchResultInner {
                match_is_bye: true, ..
            } => MatchResult::Complete(MatchResultCompleted::Bye),
            MatchResultInner { complete: None, .. } => MatchResult::InProgress,
            MatchResultInner {
                complete: Some(result),
                ..
            } => MatchResult::Complete(result),
        })
    }
}

#[derive(Debug, Deserialize)]
struct Match {
    id: u32,
    status: MatchStatus,
    #[serde(deserialize_with = "get_table_number")]
    table_number: Option<u32>,
    player_match_relationships: Vec<PlayerMatchRelationship>,

    #[serde(flatten)]
    results: MatchResult,
}

fn get_table_number<'de, D>(d: D) -> Result<Option<u32>, <D as Deserializer<'de>>::Error>
where
    D: Deserializer<'de>,
{
    if let Ok(number) = Deserialize::deserialize(d) {
        Ok(Some(number))
    } else {
        Ok(None)
    }
}

#[derive(Debug, Deserialize)]
struct Matches {
    matches: Vec<Match>,
}

impl Paginated for Matches {
    type Single = Match;

    fn page_url(id: u32, page_size: usize, page: u32) -> Url {
        matches_url(id, page_size, page, None)
    }

    fn construct(pages: Vec<Vec<Match>>) -> Self {
        Matches {
            matches: pages
                .into_iter()
                .flat_map(|page| page.into_iter())
                .collect(),
        }
    }
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
enum GenerationStatus {
    Generated,
    NotGenerated,
    #[serde(untagged)]
    Unknown(String),
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
enum RoundStatus {
    Complete,
    InProgress,
    Upcoming,
    #[serde(untagged)]
    Unknown(String),
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
enum RoundRoundType {
    PlayVsOpponent,
    #[serde(untagged)]
    Unknown(String),
}

#[derive(Debug, Deserialize)]
struct PhaseRound {
    final_round_in_event: bool,
    id: u32,
    pairings_status: GenerationStatus,
    standings_status: GenerationStatus,
    round_number: u32,
    round_type: RoundRoundType,
    status: RoundStatus,

    #[serde(flatten)]
    other: HashMap<String, serde_json::Value>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
enum PhaseRoundType {
    Swiss,
    RankedSingleElimination,
    #[serde(other)]
    Unknown,
}

#[derive(Debug, Deserialize)]
struct TournamentPhase {
    id: u32,
    phase_name: String,
    round_type: PhaseRoundType,
    status: RoundStatus,
    rounds: Vec<PhaseRound>,

    #[serde(flatten)]
    other: HashMap<String, serde_json::Value>,
}

#[derive(Debug, Deserialize)]
struct Event {
    name: String,
    tournament_phases: Vec<TournamentPhase>,
    starting_player_count: u32,

    #[serde(flatten)]
    other: HashMap<String, serde_json::Value>,
}

#[derive(Debug, Parser)]
struct Cmd {
    tourney_url: String,

    #[clap(long)]
    by_legend: Option<String>,

    #[clap(long)]
    by_rank: Option<usize>,
}

async fn get_cached<T: IntoUrl, Data: DeserializeOwned>(url: T) -> anyhow::Result<Data> {
    static CACHE: Lazy<Arc<Mutex<TimedCache<Url, String>>>> = Lazy::new(|| {
        Arc::new(Mutex::new(TimedCache::with_lifespan(Duration::from_mins(
            15,
        ))))
    });

    let url = url.into_url().context("Not a valid url")?;
    let mut cache = CACHE.lock().await;

    if let Some(result) = cache.cache_get(&url) {
        match serde_json::from_str(result) {
            Ok(parsed) => return Ok(parsed),
            Err(e) => {
                error!("Could not parse cached data: {e}.");
                warn!("Invalidating cache for {url}");
                cache.cache_remove(&url);
            }
        }
    }

    info!("Fetching {url} (not cached)");
    let result = reqwest::get(url.clone())
        .await
        .with_context(|| format!("failed to load url: {url}"))?
        .text()
        .await
        .with_context(|| format!("could not read body of {url}"))?;

    let result_parsed = serde_json::from_str(&result.clone())?;

    cache.cache_set(url, result);

    Ok(result_parsed)
}

async fn get_tournament(event_id: u32) -> anyhow::Result<Event> {
    get_cached(event_url(event_id)).await
}

async fn get_paginated<P: Paginated>(id: u32, max: Option<usize>) -> anyhow::Result<P> {
    let mut pages = Vec::new();

    let mut total = 0;
    let mut next_page = Some(1_u32);
    while let Some(page) = next_page {
        let req = P::page_url(
            id,
            if let Some(max) = max {
                usize::min(500, max - total)
            } else {
                500
            },
            page,
        );
        let page: PaginationResult<<P as Paginated>::Single> = get_cached(req).await?;
        total += page.results.len();
        pages.push(page.results);

        next_page = page.next_page_number;

        if let Some(max) = max {
            if total >= max {
                break;
            }
        }
    }

    Ok(P::construct(pages))
}

#[derive(Default, Serialize, Deserialize)]
struct ChannelData {
    current_event: Option<u32>,
}

#[derive(Default, Serialize, Deserialize)]
struct GuildData {
    channels: BTreeMap<serenity::ChannelId, ChannelData>,
}

#[derive(Default, Serialize, Deserialize)]
struct Data {
    guilds: BTreeMap<Option<serenity::GuildId>, GuildData>,
}

struct GlobalState {
    data: RwLock<Data>,
}

type Context<'a> = poise::Context<'a, GlobalState, anyhow::Error>;

trait ContextExt {
    async fn channel_data(&self) -> Option<RwLockReadGuard<'_, ChannelData>>;
    async fn channel_data_mut(&self) -> RwLockMappedWriteGuard<'_, ChannelData>;
    async fn persist_data(&self) -> anyhow::Result<()>;
}
impl<'a> ContextExt for Context<'a> {
    async fn channel_data(&self) -> Option<RwLockReadGuard<'_, ChannelData>> {
        RwLockReadGuard::try_map(self.data().data.read().await, |data| {
            data.guilds
                .get(&self.guild_id())?
                .channels
                .get(&self.channel_id())
        })
        .ok()
    }

    async fn channel_data_mut(&self) -> RwLockMappedWriteGuard<'_, ChannelData> {
        RwLockWriteGuard::map(self.data().data.write().await, |data| {
            data.guilds
                .entry(self.guild_id())
                .or_insert(GuildData::default())
                .channels
                .entry(self.channel_id())
                .or_insert(ChannelData::default())
        })
    }

    async fn persist_data(&self) -> anyhow::Result<()> {
        let mut output = std::fs::File::create(REGISTRY).context("cannot open registry file")?;
        serde_json::to_writer_pretty(&mut output, &*self.data().data.read().await)
            .context("could not write to json")?;

        Ok(())
    }
}

#[poise::command(slash_command)]
async fn current_event(ctx: Context<'_>, event_url: String) -> anyhow::Result<()> {
    ctx.defer_ephemeral().await?;

    let event_re =
        regex::Regex::new(r"(https?://)?locator.riftbound.uvsgames.com/events/(?<id>[^/]*)")
            .expect("bad regex");

    {
        let mut data = ctx.channel_data_mut().await;
        data.current_event = Some(if let Some(caps) = event_re.captures(&event_url) {
            caps["id"].parse().context("event id is not a number")?
        } else {
            return Err(anyhow::anyhow!("Could not parse provided tournament url"));
        });

        ctx.reply(format!("Tournament: {:?}", data.current_event))
            .await?;
    }

    ctx.persist_data().await?;

    Ok(())
}

fn write_row<E: ToString, I: IntoIterator<Item = E>, W: Write>(
    writer: &mut W,
    row: I,
) -> anyhow::Result<()> {
    let mut first = true;
    for header in row {
        if first {
            first = false;
            write!(writer, "{}", header.to_string())?;
        } else {
            write!(writer, "\t{}", header.to_string())?;
        }
    }
    writeln!(writer, "")?;

    Ok(())
}

fn make_table<
    HS: ToString,
    H: IntoIterator<Item = HS>,
    RS: ToString,
    R: IntoIterator<Item = RS>,
    Rows: IntoIterator<Item = R>,
>(
    headers: H,
    rows: Rows,
) -> anyhow::Result<String> {
    let mut tw = TabWriter::new(vec![]).padding(2);
    write_row(&mut tw, headers)?;
    for row in rows {
        write_row(&mut tw, row)?;
    }
    tw.flush()?;
    let written = String::from_utf8(tw.into_inner()?)?;
    let mut lines = written.lines();
    let header = lines.next().unwrap();
    let mut body = String::new();
    let mut longest = header.len();
    for line in lines {
        longest = usize::max(longest, line.len());
        writeln!(&mut body, "{line}")?;
    }

    Ok(format!("{header}\n{}\n{body}", "─".repeat(longest)))
}

#[poise::command(slash_command)]
async fn standings(
    ctx: Context<'_>,
    player: Option<String>,
    count: Option<usize>,
) -> anyhow::Result<()> {
    let locked = ctx
        .channel_data()
        .await
        .ok_or_else(|| anyhow::anyhow!("No event configured"))?;
    let event_id = locked
        .current_event
        .ok_or_else(|| anyhow::anyhow!("No event configured"))?;

    let event = get_tournament(event_id).await?;

    ctx.defer().await?;

    let completed_phase = event
        .tournament_phases
        .iter()
        .rfind(|p| {
            matches!(p.status, RoundStatus::Complete | RoundStatus::InProgress)
                && p.rounds
                    .iter()
                    .any(|r| matches!(r.status, RoundStatus::Complete))
        })
        .ok_or_else(|| anyhow::anyhow!("No candidate phase found"))?;

    let complete_round = completed_phase
        .rounds
        .iter()
        .rfind(|p| matches!(p.status, RoundStatus::Complete))
        .ok_or_else(|| anyhow::anyhow!("No complete round found"))?;

    let standings: Standings = if let Some(player) = player {
        let result: PaginationResult<StandingsResult> = get_cached(standings_url(
            complete_round.id,
            count.unwrap_or(20),
            1,
            Some(&player),
        ))
        .await?;
        Standings {
            standings: result.results,
        }
    } else {
        get_paginated(complete_round.id, Some(count.unwrap_or(20))).await?
    };

    let table = make_table(
        ["#", "Player", "Record", "Legend"],
        standings.standings.iter().map(|standing| {
            [
                standing.rank.to_string(),
                standing.user_event_status.best_identifier.clone(),
                standing.record.clone(),
                standing
                    .user_event_status
                    .deck_defining_card
                    .as_ref()
                    .map(|ddc| ddc.name.as_str())
                    .unwrap_or("UNKNOWN LEGEND")
                    .to_string(),
            ]
        }),
    )?;

    ctx.reply(format!(
        "# {}\nStandings as of round {}```\n{table}\n```",
        event.name, complete_round.round_number,
    ))
    .await?;

    Ok(())
}

#[poise::command(slash_command)]
async fn journey(ctx: Context<'_>, player: String) -> anyhow::Result<()> {
    ctx.defer().await?;

    let locked = ctx
        .channel_data()
        .await
        .ok_or_else(|| anyhow::anyhow!("No event configured"))?;
    let event_id = locked
        .current_event
        .ok_or_else(|| anyhow::anyhow!("No event configured"))?;

    let event = get_tournament(event_id).await?;

    let mut journey = Vec::new();
    for phase in event.tournament_phases {
        for round in phase.rounds {
            if matches!(round.status, RoundStatus::Complete) {
                let mut result: PaginationResult<Match> =
                    get_cached(matches_url(round.id, 1, 1, Some(&player))).await?;
                if !result.results.is_empty() {
                    journey.push(result.results.remove(0));
                }
            }
        }
    }

    let table = make_table(
        ["Round", "Record", "Opponent", "Legend"],
        journey.into_iter().enumerate().map(|(i, result)| {
            if matches!(
                result.results,
                MatchResult::Complete(MatchResultCompleted::Bye)
            ) {
                [
                    (i + 1).to_string(),
                    "BYE".to_string(),
                    "-".to_string(),
                    "-".to_string(),
                ]
            } else {
                let opp = if result.player_match_relationships[0]
                    .user_event_status
                    .best_identifier
                    == player
                {
                    &result.player_match_relationships[1]
                } else {
                    &result.player_match_relationships[0]
                };

                let record = match result.results {
                    MatchResult::Complete(MatchResultCompleted::Tie { .. }) => "TIE".to_string(),
                    MatchResult::Complete(MatchResultCompleted::Win {
                        games_won_by_winner,
                        games_won_by_loser,
                        winning_player,
                        ..
                    }) => {
                        if opp.user_event_status.user.id == winning_player {
                            format!("{games_won_by_loser}:{games_won_by_winner} (L)")
                        } else {
                            format!("{games_won_by_winner}:{games_won_by_loser} (W)")
                        }
                    }
                    _ => "?".to_string(),
                };

                [
                    (i + 1).to_string(),
                    record,
                    opp.user_event_status.best_identifier.clone(),
                    opp.user_event_status
                        .deck_defining_card
                        .as_ref()
                        .map(|ddc| ddc.name.as_str())
                        .unwrap_or("-")
                        .to_string(),
                ]
            }
        }),
    )?;

    ctx.reply(format!("# {player}'s Journey\n```\n{table}\n```",))
        .await?;

    Ok(())
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_max_level(tracing::Level::INFO)
        .init();

    dotenvy::dotenv().context("loading .env")?;

    let token = std::env::var("DISCORD_TOKEN").expect("missing DISCORD_TOKEN");
    let intents = serenity::GatewayIntents::non_privileged();

    let framework = poise::Framework::builder()
        .options(poise::FrameworkOptions {
            commands: vec![current_event(), standings(), journey()],
            ..Default::default()
        })
        .setup(|ctx, _ready, framework| {
            Box::pin(async move {
                poise::builtins::register_globally(ctx, &framework.options().commands).await?;
                let data: Data = match std::fs::read_to_string(REGISTRY) {
                    Ok(contents) => serde_json::from_str(&contents)?,
                    Err(_) => Data::default(),
                };
                Ok(GlobalState {
                    data: RwLock::new(data),
                })
            })
        })
        .build();

    let client = serenity::ClientBuilder::new(token, intents)
        .framework(framework)
        .await;
    client.unwrap().start().await.unwrap();

    Ok(())
}
