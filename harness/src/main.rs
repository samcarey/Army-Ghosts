//! `selfplay` — measure one [`BotProfile`] against another by playing them.
//!
//! ```text
//! tools/selfplay.sh --candidate accuracy=0.9,reaction=8
//! tools/selfplay.sh -c aggression=0.9 -b aggression=0.1 --ticks 1800
//! ```
//!
//! Eight bots in the real arena, four per side, for a minute of game time.
//! Every split of the eight spawn points is played from both sides with the
//! same dice (a *pair*), scored on kills minus deaths, and fed to a sequential
//! test that stops as soon as the answer is clear — see [`sprt`].
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

use army_ghosts_sim::{BotProfile, FP, MAX_PLAYERS, MEMORY_TICKS, TICK_HZ};

use game::{play, splits, Sides};
use sprt::{elo, Sprt, Verdict};

/// A minute of game time. Long enough for several engagements each and for a
/// bad early trade to be recoverable; short enough that a pair is seconds.
const DEFAULT_TICKS: usize = 60 * TICK_HZ;

/// Stop here whatever the LLR says, and report the interval instead of a
/// verdict. Two profiles that need more than this to separate are separated by
/// less than is worth acting on.
const DEFAULT_PAIRS: usize = 200;

struct Args {
    candidate: BotProfile,
    baseline: BotProfile,
    ticks: usize,
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
      --ticks N          match length in ticks   (default: 3600 = 60 s)
      --pairs N          give up after N pairs   (default: 200)
      --jobs N           matches in parallel     (default: cores - 1)
      --p1 F             win rate worth detecting (default: 0.60, ~+70 elo)
      --alpha F --beta F error rates             (default: 0.05 each)
  -q, --quiet            verdict only, no per-pair lines

SPEC is comma separated `key=value`, filling in from the default profile:
  skill, accuracy, aggression, caution   0.0 .. 1.0
  reaction                               ticks, 0 .. 23

A pair is one split of the eight spawn points played from both sides with the
same dice. Score is kills minus deaths; the sign of the pair's total is the
result.";

fn parse_args() -> Result<Args, String> {
    let mut args = Args {
        candidate: BotProfile::default(),
        baseline: BotProfile::default(),
        ticks: DEFAULT_TICKS,
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
            "--ticks" => args.ticks = number(&value()?)?,
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

/// One pair: the same split, the same dice, played from both sides.
///
/// The candidate's score is its differential in both matches added together, so
/// whatever the split was worth to whoever held it cancels. This is the only
/// reason a run of a few dozen pairs can say anything — an unpaired comparison
/// on an arena with asymmetric cover is mostly measuring the spawn points.
fn play_pair(
    candidate: BotProfile,
    baseline: BotProfile,
    ticks: usize,
    sides: &Sides,
    salt: u32,
) -> (i32, u32) {
    let mut mirrored = *sides;
    for side in mirrored.iter_mut() {
        *side = !*side;
    }
    let first = play(candidate, baseline, sides, salt, ticks);
    let second = play(candidate, baseline, &mirrored, salt, ticks);
    (
        first.net(sides) + second.net(&mirrored),
        first.total_kills() + second.total_kills(),
    )
}

fn run(args: Args) {
    let splits = splits();
    let mut sprt = Sprt::new(args.p1, args.alpha, args.beta);
    let started = Instant::now();

    println!(
        "selfplay: {} bots, {} ticks ({:.0} s) a match, arena",
        MAX_PLAYERS,
        args.ticks,
        args.ticks as f64 / TICK_HZ as f64
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
    // nothing and every cycle through the 70 splits replays the same 70 pairs.
    // Worth saying out loud: the run would still print a confident verdict.
    if args.candidate.accuracy >= FP && args.baseline.accuracy >= FP && args.pairs > splits.len() {
        println!(
            "  NOTE both profiles have accuracy 1.00, so they never touch their dice and the\n\
             \x20      salt changes nothing — only {} distinct pairs exist. Capping there.",
            splits.len()
        );
    }
    let limit = if args.candidate.accuracy >= FP && args.baseline.accuracy >= FP {
        args.pairs.min(splits.len())
    } else {
        args.pairs
    };
    println!();

    let next = AtomicUsize::new(0);
    let mut played = 0usize;
    let mut kills = 0u32;

    // Pairs are independent, so they run in parallel — but the test is
    // sequential, so results are folded in strictly in index order and the
    // verdict is checked after each. A batch already in flight when a bound is
    // crossed is finished and counted rather than discarded; throwing away
    // games you have already played because you don't like the timing is how a
    // sequential test stops controlling its error rate.
    while played < limit && sprt.verdict() == Verdict::Undecided {
        let batch = args.jobs.min(limit - played);
        let results: Vec<(usize, i32, u32)> = std::thread::scope(|scope| {
            let handles: Vec<_> = (0..batch)
                .map(|_| {
                    let next = &next;
                    let args = &args;
                    let splits = &splits;
                    scope.spawn(move || {
                        let index = next.fetch_add(1, Ordering::Relaxed);
                        let sides = splits[index % splits.len()];
                        let salt = (index / splits.len()) as u32 + 1;
                        let (margin, kills) =
                            play_pair(args.candidate, args.baseline, args.ticks, &sides, salt);
                        (index, margin, kills)
                    })
                })
                .collect();
            let mut out: Vec<_> = handles.into_iter().map(|h| h.join().unwrap()).collect();
            out.sort_unstable_by_key(|&(index, ..)| index);
            out
        });

        for (index, margin, pair_kills) in results {
            sprt.push(margin);
            played += 1;
            kills += pair_kills;
            if !args.quiet {
                let (lo, hi) = sprt.interval();
                println!(
                    "  pair {:>3}  {:>+3}   W{:<3} L{:<3} T{:<3}  rate {:.3} [{:.2}-{:.2}]  LLR {:+.3}",
                    index + 1,
                    margin,
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
    println!(
        "  {played} pairs ({} matches, {kills} kills) in {:.1} s at {:.0} ms a match",
        played * 2,
        elapsed.as_secs_f64(),
        elapsed.as_secs_f64() * 1000.0 / (played * 2).max(1) as f64
    );
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
    /// that takes 380, on the same splits with the same dice — and requires it
    /// to say so. A harness that cannot separate those two is measuring
    /// something other than fighting, and every number it ever printed was
    /// noise dressed as evidence.
    ///
    /// Short matches and one thread, because it runs in the default (debug)
    /// test build. It is still the slowest test in the repo by some way.
    #[test]
    fn the_harness_separates_a_quick_bot_from_a_slow_one() {
        let quick = parse_profile("reaction=3").expect("parse");
        let slow = parse_profile("reaction=23").expect("parse");
        let splits = splits();
        let mut sprt = Sprt::new(0.6, 0.05, 0.05);
        for sides in splits.iter().take(40) {
            if sprt.verdict() != Verdict::Undecided {
                break;
            }
            let (margin, _) = play_pair(quick, slow, 600, sides, 1);
            sprt.push(margin);
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
    /// profiles cancel exactly under the pairing — the mirrored match is the
    /// same match with the labels swapped — so every pair is a tie and the test
    /// can never reach a bound. A harness that drifted to a verdict here would
    /// declare a winner between two copies of the same bot.
    #[test]
    fn identical_profiles_tie_every_time() {
        let profile = BotProfile::default();
        let splits = splits();
        let mut sprt = Sprt::new(0.6, 0.05, 0.05);
        for sides in splits.iter().take(6) {
            let (margin, kills) = play_pair(profile, profile, 600, sides, 1);
            assert_eq!(margin, 0, "identical profiles produced a margin");
            assert!(kills > 0, "nobody fired a shot; the tie means nothing");
            sprt.push(margin);
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
