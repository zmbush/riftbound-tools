use std::collections::HashMap;

use anyhow::Context as _;
use clap::Parser;
use once_cell::sync::Lazy;
use reqwest::Url;
use serde::{de::DeserializeOwned, Deserialize, Deserializer};

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
fn matches_url(event_id: u32, page_size: u32, page: u32) -> Url {
    let mut url = tournament_rounds_url(event_id)
        .join("matches/paginated/")
        .expect("invalid path");

    url.query_pairs_mut()
        .append_pair("page_size", &page_size.to_string())
        .append_pair("page", &page.to_string());

    url
}
fn standings_url(event_id: u32, page_size: u32, page: u32) -> Url {
    let mut url = tournament_rounds_url(event_id)
        .join("standings/paginated/")
        .expect("invalid path");

    url.query_pairs_mut()
        .append_pair("page_size", &page_size.to_string())
        .append_pair("page", &page.to_string());

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

    fn page_url(id: u32, page_size: u32, page: u32) -> Url;
    fn construct(pages: Vec<Vec<Self::Single>>) -> Self;
}

impl Paginated for Standings {
    type Single = StandingsResult;

    fn page_url(id: u32, page_size: u32, page: u32) -> Url {
        standings_url(id, page_size, page)
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
    results: Option<MatchResult>,
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

    fn page_url(id: u32, page_size: u32, page: u32) -> Url {
        matches_url(id, page_size, page)
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

async fn get_tournament(event_id: u32) -> anyhow::Result<Event> {
    Ok(reqwest::get(event_url(event_id))
        .await
        .context("failed to load tournament info")?
        .json()
        .await?)
}

async fn get_paginated<P: Paginated>(id: u32, max: Option<usize>) -> anyhow::Result<P> {
    let mut pages = Vec::new();

    let mut total = 0;
    let mut next_page = Some(1);
    while let Some(page) = next_page {
        let req = P::page_url(id, 500, page);
        println!("Requesting: {req}");
        let page: PaginationResult<<P as Paginated>::Single> = reqwest::get(req)
            .await
            .context("Could not load page")?
            .json()
            .await
            .context("Failed to parse page")?;
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

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let args = Cmd::parse();
    let event_re =
        regex::Regex::new(r"(https://)?locator.riftbound.uvsgames.com/events/(?<id>[^/]*)")
            .expect("bad regex");

    let event_id = if let Some(caps) = event_re.captures(&args.tourney_url) {
        caps["id"].parse().context("event id is not a number")?
    } else {
        return Err(anyhow::anyhow!("Could not parse provided tournament url"));
    };

    let tournament = get_tournament(event_id).await?;

    println!("Reading standings for: {}", tournament.name);

    let completed_phase = tournament
        .tournament_phases
        .iter()
        .rfind(|p| {
            matches!(p.status, RoundStatus::Complete | RoundStatus::InProgress)
                && p.rounds
                    .iter()
                    .any(|r| matches!(r.status, RoundStatus::Complete))
        })
        .ok_or_else(|| anyhow::anyhow!("No candidate phase found"))?;
    let running_phase = tournament
        .tournament_phases
        .iter()
        .rfind(|p| {
            matches!(p.status, RoundStatus::Complete | RoundStatus::InProgress)
                && p.rounds
                    .iter()
                    .any(|r| matches!(r.status, RoundStatus::Complete | RoundStatus::InProgress))
        })
        .ok_or_else(|| anyhow::anyhow!("No in-progress phase"))?;

    println!("Phase: {}", completed_phase.phase_name);

    let complete_round = completed_phase
        .rounds
        .iter()
        .rfind(|p| matches!(p.status, RoundStatus::Complete))
        .ok_or_else(|| anyhow::anyhow!("No complete round found"))?;
    let running_round = running_phase
        .rounds
        .iter()
        .rfind(|p| matches!(p.status, RoundStatus::Complete | RoundStatus::InProgress))
        .ok_or_else(|| anyhow::anyhow!("No running round found"))?;

    println!("Completed Round {}", complete_round.round_number);
    println!("Running Round {}", running_round.round_number);

    let matches: Matches = get_paginated(running_round.id, None).await?;
    for mat in matches.matches {
        println!("{} -> {:?}", mat.id, mat.results);
    }
    return Ok(());

    let standings: Standings = get_paginated(complete_round.id, args.by_rank).await?;

    for standing in standings.standings.iter().filter(|s| {
        if let Some(legend) = &args.by_legend {
            if let Some(player_legend) = s
                .user_event_status
                .deck_defining_card
                .as_ref()
                .map(|ddc| &ddc.name)
            {
                if legend != player_legend {
                    return false;
                }
            } else {
                return false;
            }
        }

        if let Some(rank) = args.by_rank {
            if s.rank > rank {
                return false;
            }
        }

        true
    }) {
        println!(
            "{} - {} - {} ({}): {}",
            standing.rank,
            standing.record,
            standing.user_event_status.best_identifier,
            standing.player.best_identifier,
            standing
                .user_event_status
                .deck_defining_card
                .as_ref()
                .map(|ddc| ddc.name.as_str())
                .unwrap_or("UNKNOWN LEGEND")
        );
    }

    Ok(())
}
