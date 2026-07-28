//! Wald's sequential probability ratio test: stop as soon as the evidence is
//! in, rather than after a round number of matches.
//!
//! # Why sequentially, and why on pairs
//!
//! A fixed-length run has to be sized for the smallest difference worth
//! detecting, so proving that a blatantly better bot is better costs the same
//! as proving a marginal one is. An SPRT accumulates a log-likelihood ratio and
//! stops the moment it crosses a bound, which for an obvious difference is a
//! couple of dozen games and for a marginal one is as many as it takes. Since a
//! match here costs real seconds, that is most of what makes the harness usable.
//!
//! The trials are **pairs**, not matches. The arena is not symmetric — spawn
//! points sit in different cover — so which four seats a profile draws is worth
//! something on its own. Playing every split from both sides and scoring the
//! two together cancels that exactly, which is the same variance reduction
//! chess engine testing gets from playing both colours of an opening.
//!
//! # The hypotheses
//!
//! `H0: p = 0.5` — the candidate is no better. `H1: p = p1` — it wins that
//! share of decisive pairs. Stated as a win rate rather than in elo on purpose:
//! elo is a chess convention calibrated to chess draw rates, and quoting a
//! bot's shooting as "+35 elo" would be borrowing a precision this doesn't
//! have. (It converts, and [`elo`] does it, for anyone who thinks in it.)
//!
//! Tied pairs are dropped and the test is on the decisive ones, which is
//! Wald's binomial SPRT unmodified — no draw model to get wrong. What it costs
//! is that a candidate which turns wins into ties looks identical to one that
//! changes nothing, so the tie count is reported rather than swept up.

/// A running sequential test over decisive trials.
#[derive(Copy, Clone, Debug)]
pub struct Sprt {
    /// Win rate under H0 — always 0.5 here ("no better than the baseline").
    pub p0: f64,
    /// Win rate under H1, the difference worth detecting.
    pub p1: f64,
    /// Cross this and H1 is accepted: the candidate is better.
    pub upper: f64,
    /// Cross this and H0 is accepted: it isn't.
    pub lower: f64,
    pub wins: u32,
    pub losses: u32,
    pub ties: u32,
    llr: f64,
}

#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum Verdict {
    /// The candidate is better by at least the margin asked for.
    Better,
    /// It isn't — either no different or worse.
    NotBetter,
    /// Not enough evidence yet.
    Undecided,
}

impl Sprt {
    /// `p1` is the alternative win rate; `alpha` and `beta` are the false
    /// positive and false negative rates the bounds are set from.
    pub fn new(p1: f64, alpha: f64, beta: f64) -> Self {
        assert!(p1 > 0.5 && p1 < 1.0, "p1 must be a win rate above a half");
        assert!(alpha > 0.0 && alpha < 0.5 && beta > 0.0 && beta < 0.5);
        Self {
            p0: 0.5,
            p1,
            upper: ((1.0 - beta) / alpha).ln(),
            lower: (beta / (1.0 - alpha)).ln(),
            wins: 0,
            losses: 0,
            ties: 0,
            llr: 0.0,
        }
    }

    /// Record one trial. `margin` is the candidate's score for the pair; its
    /// sign is the result and zero is a tie.
    pub fn push(&mut self, margin: i32) {
        match margin.signum() {
            1 => {
                self.wins += 1;
                self.llr += (self.p1 / self.p0).ln();
            }
            -1 => {
                self.losses += 1;
                self.llr += ((1.0 - self.p1) / (1.0 - self.p0)).ln();
            }
            _ => self.ties += 1,
        }
    }

    pub fn llr(&self) -> f64 {
        self.llr
    }

    pub fn decisive(&self) -> u32 {
        self.wins + self.losses
    }

    pub fn verdict(&self) -> Verdict {
        if self.llr >= self.upper {
            Verdict::Better
        } else if self.llr <= self.lower {
            Verdict::NotBetter
        } else {
            Verdict::Undecided
        }
    }

    /// Observed win rate among decisive pairs.
    pub fn rate(&self) -> f64 {
        if self.decisive() == 0 {
            return 0.5;
        }
        self.wins as f64 / self.decisive() as f64
    }

    /// Wilson score interval on that rate — reported because a verdict of
    /// "undecided after 200 pairs" is far more useful with a range attached
    /// than without one. 95%, so `z` is 1.96.
    pub fn interval(&self) -> (f64, f64) {
        let n = self.decisive() as f64;
        if n == 0.0 {
            return (0.0, 1.0);
        }
        let z = 1.959_964;
        let p = self.rate();
        let denom = 1.0 + z * z / n;
        let centre = (p + z * z / (2.0 * n)) / denom;
        let half = z * ((p * (1.0 - p) / n + z * z / (4.0 * n * n)).sqrt()) / denom;
        ((centre - half).max(0.0), (centre + half).min(1.0))
    }
}

/// A win rate as elo, for anyone who wants the familiar number. The logistic
/// scale, same as chess: 0.5 is 0, 0.6 is about +70.
pub fn elo(p: f64) -> f64 {
    if p <= 0.0 {
        return f64::NEG_INFINITY;
    }
    if p >= 1.0 {
        return f64::INFINITY;
    }
    -400.0 * (1.0 / p - 1.0).log10()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The bounds are Wald's, and they are what the whole error control rests
    /// on: at alpha = beta = 0.05 they are ±ln(19).
    #[test]
    fn the_bounds_are_walds() {
        let sprt = Sprt::new(0.6, 0.05, 0.05);
        assert!((sprt.upper - 19.0f64.ln()).abs() < 1e-12);
        assert!((sprt.lower + 19.0f64.ln()).abs() < 1e-12);
    }

    /// A run of wins accepts H1, a run of losses accepts H0, and neither
    /// happens before there is enough of one.
    #[test]
    fn a_streak_decides_and_a_short_one_does_not() {
        let mut sprt = Sprt::new(0.6, 0.05, 0.05);
        sprt.push(1);
        assert_eq!(sprt.verdict(), Verdict::Undecided, "one pair decided a match-up");
        for _ in 0..40 {
            sprt.push(1);
        }
        assert_eq!(sprt.verdict(), Verdict::Better);

        let mut sprt = Sprt::new(0.6, 0.05, 0.05);
        for _ in 0..40 {
            sprt.push(-1);
        }
        assert_eq!(sprt.verdict(), Verdict::NotBetter);
    }

    /// Even split, no verdict — however long it runs. This is the one that
    /// matters: two identical profiles must not come out separable, and a sign
    /// error in the LLR would make them drift to a bound.
    #[test]
    fn an_even_split_never_decides_better() {
        let mut sprt = Sprt::new(0.6, 0.05, 0.05);
        for _ in 0..500 {
            sprt.push(1);
            sprt.push(-1);
        }
        assert_ne!(sprt.verdict(), Verdict::Better);
        assert!((sprt.rate() - 0.5).abs() < 1e-9);
    }

    /// Ties are counted and are not evidence either way.
    #[test]
    fn ties_move_nothing() {
        let mut sprt = Sprt::new(0.6, 0.05, 0.05);
        for _ in 0..100 {
            sprt.push(0);
        }
        assert_eq!(sprt.ties, 100);
        assert_eq!(sprt.decisive(), 0);
        assert_eq!(sprt.llr(), 0.0);
        assert_eq!(sprt.verdict(), Verdict::Undecided);
    }

    /// The elo conversion is the standard logistic one.
    #[test]
    fn elo_is_the_usual_curve() {
        assert!(elo(0.5).abs() < 1e-9);
        assert!((elo(0.6) - 70.437).abs() < 0.01);
        assert!(elo(0.4) < 0.0);
    }

    /// The interval brackets the observed rate and tightens with evidence.
    #[test]
    fn the_interval_narrows() {
        let mut few = Sprt::new(0.6, 0.05, 0.05);
        let mut many = Sprt::new(0.6, 0.05, 0.05);
        for _ in 0..10 {
            few.push(1);
            few.push(-1);
        }
        for _ in 0..400 {
            many.push(1);
            many.push(-1);
        }
        let (flo, fhi) = few.interval();
        let (mlo, mhi) = many.interval();
        assert!(flo < 0.5 && fhi > 0.5 && mlo < 0.5 && mhi > 0.5);
        assert!(mhi - mlo < fhi - flo);
    }
}
