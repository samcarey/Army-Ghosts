//! `selfplay` — measure one [`BotProfile`] against another by playing them.
//!
//! ```text
//! tools/selfplay.sh --candidate accuracy=0.9,reaction=8
//! tools/selfplay.sh -c aggression=0.9 -b aggression=0.1 --rounds 5
//! ```
//!
//! Eight bots in the real arena, four per side, playing Ghost War rounds:
//! muster at opposite ends, two minutes or until one side is wiped out, and
//! **nobody respawns**. A *pair* is the same dice played with the candidate on
//! each end of the field in turn; it is scored on **rounds won minus rounds
//! lost** and fed to a sequential test that stops as soon as the answer is clear
//! — see [`sprt`].
//!
//! # What this is for
//!
//! `BotProfile`'s five numbers were picked, not measured. This is the thing
//! that turns any one of them into a claim that can be checked: change a dial,
//! run it against the default, and find out whether the change is worth
//! anything or is noise. It answers "is this better", which is the question
//! tuning actually asks — it is not a fitness function to breed against, and
//! it deliberately has no search loop bolted on. A profile that wins here wins
//! at fighting seven other bots in this arena; that is a real fact and it is
//! also the only fact it establishes.

mod game;
mod sprt;

use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Instant;

use army_ghosts_sim::{
    BotProfile, FP, INTERMISSION_TICKS, MAX_PLAYERS, MEMORY_TICKS, ROUND_SECONDS, TICK_HZ,
};

use game::play;
use sprt::{elo, Sprt, Verdict};

/// Rounds per match, so eighteen a pair.
///
/// **This is set by the tie rate, and the tie rate is the thing to understand
/// about this harness.** A pair's score is a small integer — rounds won minus
/// rounds lost, added across the two orientations — so unlike the old
/// kills-minus-deaths differential it lands on exactly zero a great deal of the
/// time, and every tie is a pair the sequential test drops. Measured against a
/// caution=0.1 candidate: 3 rounds a match left 3 of 60 pairs decisive, 9 left
/// 12, 15 left 13. So it is worth paying for the climb out of 3 and not worth
/// paying for the plateau past 9.
///
/// The 3-round run is worth a second look, because it did not merely say less —
/// it pointed the OTHER WAY (66% on three decisive pairs, against 25% on twelve).
/// Three pairs is not a measurement, and a harness that can produce a confident
/// wrong sign from one is worse than useless.
const DEFAULT_ROUNDS: u32 = 9;

/// Stop here whatever the LLR says, and report the interval instead of a
/// verdict. Two profiles that need more than this to separate are separated by
/// less than is worth acting on. Generous, because only about a fifth of pairs
/// come out decisive — see [`DEFAULT_ROUNDS`].
const DEFAULT_PAIRS: usize = 150;

struct Args {
    candidate: BotProfile,
    baseline: BotProfile,
    rounds: u32,
    pairs: usize,
    jobs: usize,
    p1: f64,
    alpha: f64,
    beta: f64,
    quiet: bool,
}

fn main() {
    let args = match parse_args() {
        Ok(args) => args,
        Err(message) => {
            eprintln!("selfplay: {message}\n");
            eprintln!("{USAGE}");
            std::process::exit(2);
        }
    };
    run(args);
}

const USAGE: &str = "\
usage: selfplay [options]

  -c, --candidate SPEC   profile under test      (default: the shipping one)
  -b, --baseline  SPEC   what it plays against   (default: the shipping one)
      --rounds N         rounds per match        (default: 9)
      --pairs N          give up after N pairs   (default: 150)
      --jobs N           matches in parallel     (default: cores - 1)
      --p1 F             win rate worth detecting (default: 0.60, ~+70 elo)
      --alpha F --beta F error rates             (default: 0.05 each)
  -q, --quiet            verdict only, no per-pair lines

SPEC is comma separated `key=value`, filling in from the default profile:
  skill, accuracy, aggression, caution   0.0 .. 1.0
  reaction                               ticks, 0 .. 23

A pair is the same dice played twice, with the candidate on the west line and
then on the east one. Score is rounds won minus rounds lost, added across both;
the sign of the pair's total is the result.";

fn parse_args() -> Result<Args, String> {
    let mut args = Args {
        candidate: BotProfile::default(),
        baseline: BotProfile::default(),
        rounds: DEFAULT_ROUNDS,
        pairs: DEFAULT_PAIRS,
        jobs: std::thread::available_parallelism()
            .map(|n| (n.get() - 1).max(1))
            .unwrap_or(1),
        p1: 0.60,
        alpha: 0.05,
        beta: 0.05,
        quiet: false,
    };
    let mut argv = std::env::args().skip(1);
    while let Some(flag) = argv.next() {
        let mut value = || argv.next().ok_or(format!("{flag} needs a value"));
        match flag.as_str() {
            "-h" | "--help" => {
                println!("{USAGE}");
                std::process::exit(0);
            }
            "-q" | "--quiet" => args.quiet = true,
            "-c" | "--candidate" => args.candidate = parse_profile(&value()?)?,
            "-b" | "--baseline" => args.baseline = parse_profile(&value()?)?,
            "--rounds" => args.rounds = number::<u32>(&value()?)?.max(1),
            "--pairs" => args.pairs = number(&value()?)?,
            "--jobs" => args.jobs = number::<usize>(&value()?)?.max(1),
            "--p1" => args.p1 = number(&value()?)?,
            "--alpha" => args.alpha = number(&value()?)?,
            "--beta" => args.beta = number(&value()?)?,
            other => return Err(format!("unknown option {other}")),
        }
    }
    if !(0.5..1.0).contains(&args.p1) || args.p1 == 0.5 {
        return Err("--p1 must be a win rate strictly between 0.5 and 1.0".into());
    }
    Ok(args)
}

fn number<T: std::str::FromStr>(text: &str) -> Result<T, String> {
    text.parse().map_err(|_| format!("`{text}` isn't a number"))
}

/// `skill=0.8,reaction=6` onto a copy of the shipping profile. Unset dials keep
/// their default, so a spec says exactly what is being varied and nothing else
/// silently moves with it.
fn parse_profile(spec: &str) -> Result<BotProfile, String> {
    let mut profile = BotProfile::default();
    for field in spec.split(',').filter(|f| !f.trim().is_empty()) {
        let (key, raw) = field
            .split_once('=')
            .ok_or_else(|| format!("`{field}` should be key=value"))?;
        let key = key.trim();
        let raw = raw.trim();
        if key == "reaction" {
            let ticks: u32 = number(raw)?;
            if ticks as usize >= MEMORY_TICKS {
                return Err(format!(
                    "reaction {ticks} is past the {MEMORY_TICKS}-tick memory; \
                     the bot cannot remember that far back"
                ));
            }
            profile.reaction = ticks as u8;
            continue;
        }
        let unit: f64 = number(raw)?;
        if !(0.0..=1.0).contains(&unit) {
            return Err(format!("{key} is a 0..1 dial, got {unit}"));
        }
        let fixed = (unit * FP as f64).round() as i32;
        match key {
            "skill" => profile.skill = fixed,
            "accuracy" => profile.accuracy = fixed,
            "aggression" => profile.aggression = fixed,
            "caution" => profile.caution = fixed,
            other => return Err(format!("no dial called `{other}`")),
        }
    }
    Ok(profile)
}

/// The observed rate as elo, for anyone who thinks in it. A clean sweep is
/// infinite on that scale, which is a fact about the scale rather than about
/// the bot, so it says so instead of printing `inf`.
fn show_elo(rate: f64) -> String {
    let elo = elo(rate);
    if elo.is_finite() {
        format!("about {elo:+.0} elo")
    } else {
        "a clean sweep".into()
    }
}

fn show(profile: &BotProfile) -> String {
    let unit = |v: i32| v as f64 / FP as f64;
    format!(
        "skill {:.2}  accuracy {:.2}  reaction {:<2}  aggression {:.2}  caution {:.2}",
        unit(profile.skill),
        unit(profile.accuracy),
        profile.reaction,
        unit(profile.aggression),
        unit(profile.caution)
    )
}

/// What one pair produced: the candidate's margin in rounds, plus the kills and
/// ticks behind it for the cost and sanity lines.
#[derive(Copy, Clone, Debug, Default)]
struct Pair {
    margin: i32,
    kills: u32,
    ticks: usize,
    /// Rounds neither side took. Reported because it is the tell for the failure
    /// mode this scoring has and the old one didn't: two cautious profiles can
    /// spend the whole clock not finding each other, and a run full of drawn
    /// rounds is a run that measured almost nothing.
    draws: u32,
}

/// One pair: the same dice, played with the candidate on each end of the field
/// in turn.
///
/// The candidate's score is its round differential in both matches added
/// together, so whatever the end of the field was worth to whoever held it
/// cancels. This is the only reason a run of a few dozen pairs can say anything:
/// the two muster lines are exact mirrors but the rock and bush fields are NOT,
/// so an unpaired comparison is partly measuring which end a profile drew.
///
/// The old harness paired over all 70 ways of splitting eight scattered spawn
/// points; with two fixed lines there is exactly one split, and the variety has
/// to come from the salt instead. That is a real loss of an independent source
/// of variation, and it is why [`run`] says so when the salt is inert.
fn play_pair(candidate: BotProfile, baseline: BotProfile, rounds: u32, salt: u32) -> Pair {
    let west = play(candidate, baseline, 0, salt, rounds);
    let east = play(candidate, baseline, 1, salt, rounds);
    Pair {
        margin: west.net(0) + east.net(1),
        kills: west.total_kills() + east.total_kills(),
        ticks: west.ticks + east.ticks,
        draws: west.draws + east.draws,
    }
}

fn run(args: Args) {
    let mut sprt = Sprt::new(args.p1, args.alpha, args.beta);
    let started = Instant::now();

    println!(
        "selfplay: {} bots, {} a side, {} rounds a match ({} s each at most)",
        MAX_PLAYERS,
        MAX_PLAYERS / 2,
        args.rounds,
        ROUND_SECONDS
    );
    println!("  candidate  {}", show(&args.candidate));
    println!("  baseline   {}", show(&args.baseline));
    println!(
        "  H0 rate {:.3}  H1 rate {:.3}  alpha {:.2}  beta {:.2}  bounds {:+.3} .. {:+.3}",
        sprt.p0, sprt.p1, args.alpha, args.beta, sprt.lower, sprt.upper
    );
    if args.candidate == args.baseline {
        println!("  NOTE both profiles are identical — this measures the harness, not a bot.");
    }
    // A bot that never misses never draws from its RNG, so the salt reaches
    // nothing — and the salt is now the ONLY thing that varies between pairs, so
    // every pair is the same pair. Worth saying out loud: the run would happily
    // print the same margin a hundred times and a confident verdict under it.
    let dice_are_dead = args.candidate.accuracy >= FP && args.baseline.accuracy >= FP;
    if dice_are_dead && args.pairs > 1 {
        println!(
            "  NOTE both profiles have accuracy 1.00, so they never touch their dice. The salt\n\
             \x20      is the only thing that varies a pair, so exactly one distinct pair exists.\n\
             \x20      Capping there — repeating it would not be more evidence."
        );
    }
    let limit = if dice_are_dead { args.pairs.min(1) } else { args.pairs };
    println!();

    let next = AtomicUsize::new(0);
    let mut played = 0usize;
    let mut kills = 0u32;
    let mut ticks = 0usize;
    let mut drawn = 0u32;

    // Pairs are independent, so they run in parallel — but the test is
    // sequential, so results are folded in strictly in index order and the
    // verdict is checked after each. A batch already in flight when a bound is
    // crossed is finished and counted rather than discarded; throwing away
    // games you have already played because you don't like the timing is how a
    // sequential test stops controlling its error rate.
    while played < limit && sprt.verdict() == Verdict::Undecided {
        let batch = args.jobs.min(limit - played);
        let results: Vec<(usize, Pair)> = std::thread::scope(|scope| {
            let handles: Vec<_> = (0..batch)
                .map(|_| {
                    let next = &next;
                    let args = &args;
                    scope.spawn(move || {
                        let index = next.fetch_add(1, Ordering::Relaxed);
                        let salt = index as u32 + 1;
                        (index, play_pair(args.candidate, args.baseline, args.rounds, salt))
                    })
                })
                .collect();
            let mut out: Vec<_> = handles.into_iter().map(|h| h.join().unwrap()).collect();
            out.sort_unstable_by_key(|&(index, ..)| index);
            out
        });

        for (index, pair) in results {
            sprt.push(pair.margin);
            played += 1;
            kills += pair.kills;
            ticks += pair.ticks;
            drawn += pair.draws;
            if !args.quiet {
                let (lo, hi) = sprt.interval();
                println!(
                    "  pair {:>3}  {:>+3}   W{:<3} L{:<3} T{:<3}  rate {:.3} [{:.2}-{:.2}]  LLR {:+.3}",
                    index + 1,
                    pair.margin,
                    sprt.wins,
                    sprt.losses,
                    sprt.ties,
                    sprt.rate(),
                    lo,
                    hi,
                    sprt.llr()
                );
            }
        }
    }

    let elapsed = started.elapsed();
    let (lo, hi) = sprt.interval();
    println!();
    match sprt.verdict() {
        Verdict::Better => println!(
            "verdict: BETTER — the candidate wins more than {:.0}% of decisive pairs.",
            args.p1 * 100.0
        ),
        // H0 accepted means "not ahead by the margin asked for", which covers
        // both "the same" and "worse" — the rate below says which, and the
        // difference matters, so don't read this line on its own.
        Verdict::NotBetter => println!(
            "verdict: NOT BETTER — the candidate does not win {:.0}% of decisive pairs.\n\
             \x20        {}",
            args.p1 * 100.0,
            if sprt.rate() < 0.45 {
                "It lost more than it won: this change is a regression."
            } else {
                "It is about level with the baseline, not behind it."
            }
        ),
        Verdict::Undecided => println!(
            "verdict: UNDECIDED after {played} pairs — any difference is smaller than this\n\
             \x20        run can see. Widen it with --pairs, or ask a coarser question with --p1."
        ),
    }
    let matches = (played * 2).max(1);
    println!(
        "  {played} pairs ({} matches, {} rounds, {kills} kills) in {:.1} s at {:.0} ms a match",
        played * 2,
        played * 2 * args.rounds as usize,
        elapsed.as_secs_f64(),
        elapsed.as_secs_f64() * 1000.0 / matches as f64
    );
    // How long a round actually took, which is the number to look at when a run
    // is slow: a side that hides rather than advancing runs the clock out every
    // time, and a match then costs the full two minutes a round instead of the
    // twenty-odd seconds a fought round does.
    if played > 0 {
        let rounds = played * 2 * args.rounds as usize;
        // Net of the intermissions, which are in the tick count but are not the
        // round — without taking them off, the average can read as longer than
        // the clock a round is actually given, which is nonsense on its face.
        let fighting = ticks.saturating_sub(rounds * INTERMISSION_TICKS as usize);
        let per_round = fighting as f64 / rounds as f64;
        println!(
            "  rounds lasted {:.0} ticks ({:.1} s) of fighting on average, of a possible {}, \
             and {drawn} of {rounds} were drawn",
            per_round,
            per_round / TICK_HZ as f64,
            ROUND_SECONDS
        );
        if drawn * 2 > rounds as u32 {
            println!(
                "  NOTE most rounds ended level, so most of this run measured two sides not\n\
                 \x20      finding each other rather than one beating the other."
            );
        }
        // Distinct from the note above: those are rounds nobody won, these are
        // PAIRS that came out level on rounds. A run that is mostly ties has
        // thrown most of itself away, and the honest response is more rounds a
        // match rather than more pairs.
        if sprt.ties as usize * 2 > played {
            println!(
                "  NOTE most pairs came out level on rounds and were dropped. The evidence here\n\
                 \x20      is the {} decisive pairs, not the {played} played — raise --rounds \
                 before --pairs.",
                sprt.decisive()
            );
        }
    }
    println!(
        "  decisive {}: won {} ({:.1}%, 95% CI {:.1}-{:.1}%, {}), tied {}",
        sprt.decisive(),
        sprt.wins,
        sprt.rate() * 100.0,
        lo * 100.0,
        hi * 100.0,
        show_elo(sprt.rate()),
        sprt.ties
    );
    println!(
        "  LLR {:+.3} against bounds {:+.3} .. {:+.3}",
        sprt.llr(),
        sprt.lower,
        sprt.upper
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A spec changes what it names and nothing else.
    #[test]
    fn a_spec_varies_one_dial() {
        let profile = parse_profile("accuracy=0.25").expect("parse");
        let default = BotProfile::default();
        assert_eq!(profile.accuracy, FP / 4);
        assert_eq!(profile.skill, default.skill);
        assert_eq!(profile.reaction, default.reaction);
        assert_eq!(profile.aggression, default.aggression);
        assert_eq!(profile.caution, default.caution);
    }

    #[test]
    fn specs_reject_what_they_should() {
        assert!(parse_profile("accuracy=1.5").is_err(), "accepted a dial past 1");
        assert!(parse_profile("nonsense=0.5").is_err(), "accepted an unknown dial");
        assert!(parse_profile("accuracy").is_err(), "accepted a bare key");
        // A reaction past the memory would silently never recall anything.
        assert!(
            parse_profile(&format!("reaction={MEMORY_TICKS}")).is_err(),
            "accepted a reaction the memory can't reach"
        );
        assert!(parse_profile(&format!("reaction={}", MEMORY_TICKS - 1)).is_ok());
    }

    /// **Does the instrument work?**
    ///
    /// Everything else here tests the parts. This runs the whole thing on a
    /// question with a known answer — a bot that reacts in 50 ms against one
    /// that takes 380, on the same dice — and requires it to say so. A harness
    /// that cannot separate those two is measuring something other than
    /// fighting, and every number it ever printed was noise dressed as evidence.
    ///
    /// One round a match and one thread, because it runs in the default (debug)
    /// test build. It is still the slowest test in the repo by some way.
    #[test]
    fn the_harness_separates_a_quick_bot_from_a_slow_one() {
        let quick = parse_profile("reaction=3").expect("parse");
        let slow = parse_profile("reaction=23").expect("parse");
        let mut sprt = Sprt::new(0.6, 0.05, 0.05);
        for salt in 1..=40 {
            if sprt.verdict() != Verdict::Undecided {
                break;
            }
            sprt.push(play_pair(quick, slow, 1, salt).margin);
        }
        assert_eq!(
            sprt.verdict(),
            Verdict::Better,
            "the quicker bot didn't come out ahead: {} won of {} decisive, LLR {:+.3}",
            sprt.wins,
            sprt.decisive(),
            sprt.llr()
        );
    }

    /// …and it does not invent a difference where there is none. Two identical
    /// profiles cancel exactly under the pairing — with the same profile on both
    /// ends the mirrored match is literally the same simulation, so its round
    /// tally is the same tally read the other way round — and every pair is a
    /// tie. A harness that drifted to a verdict here would declare a winner
    /// between two copies of the same bot.
    #[test]
    fn identical_profiles_tie_every_time() {
        let profile = BotProfile::default();
        let mut sprt = Sprt::new(0.6, 0.05, 0.05);
        for salt in 1..=6 {
            let pair = play_pair(profile, profile, 1, salt);
            assert_eq!(pair.margin, 0, "identical profiles produced a margin");
            assert!(pair.kills > 0, "nobody fired a shot; the tie means nothing");
            sprt.push(pair.margin);
        }
        assert_eq!(sprt.ties, 6);
        assert_eq!(sprt.verdict(), Verdict::Undecided);
    }

    /// Several dials at once, and whitespace survives.
    #[test]
    fn specs_take_a_list() {
        let profile = parse_profile(" skill=1.0 , reaction=3 ,caution=0.0").expect("parse");
        assert_eq!(profile.skill, FP);
        assert_eq!(profile.reaction, 3);
        assert_eq!(profile.caution, 0);
        assert_eq!(profile.accuracy, BotProfile::default().accuracy);
    }
}
