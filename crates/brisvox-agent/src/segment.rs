//! Streaming text segmenter for TTS.
//!
//! The LLM streams text deltas; TTS wants speakable chunks. [`Segmenter`] buffers the deltas and
//! emits a chunk as soon as a boundary is safe to speak:
//!
//! - **First chunk**: the earliest clause boundary (`,` `;` `:` followed by whitespace, or a
//!   spaced dash) or sentence end, once the chunk holds at least
//!   [`SegmenterConfig::first_min_words`] words. Time to first audio matters most here.
//! - **Later chunks** end only at sentence ends (`.` `!` `?` `…`, runs such as `?!`, closing
//!   quotes, brackets or markdown emphasis after them) and at newlines after text, so prosody
//!   stays natural. A newline ends a later chunk on purpose: LLM lists and paragraphs need a pause
//!   there, and without it `1. Buy milk\n2. Call mom` becomes one run-on utterance. A line that
//!   ends in `,` `;` `:` or a dash ends its chunk at the newline too (`- eggs,\n- milk,` is two
//!   items). A line that ends in a terminator is left to the terminator rules (`1.` stays with
//!   its item).
//! - **Over the limit**: when the pending text grows past [`SegmenterConfig::max_chunk_chars`]
//!   without a sentence end, it is cut at the last clause boundary inside the limit, else at the
//!   last whitespace inside the limit, else at the first whitespace after it (the over-long token
//!   stays whole). A whitespace right after a period that is not a sentence end (`Dr.`, `3.`
//!   before `sinif`, `E.U.`) is never a fallback cut. Only when no usable whitespace shows up
//!   within 4 × `max_chunk_chars` is the text cut at a char boundary, preferably one that is not
//!   inside a grapheme (ZWJ sequences, variation selectors, combining marks of the common scripts,
//!   skin tones and flags stay whole). That hard cap bounds the pending text and the stall; it is
//!   the one exception to "cut only at whitespace or after punctuation". A single grapheme longer
//!   than the cap (e.g. a run of combining marks with no whitespace) has no safe boundary, so it
//!   is cut at the cap itself: bounded memory wins over keeping it whole.
//! - **One-token lookahead**: a period, `?`, `!`, `…` or clause mark is decided only when the
//!   next non-space character has arrived (or in [`Segmenter::finish`]). `Dr.` followed by
//!   ` Smith`, `3.` followed by `14`, `Yes.` followed by ` and` are different only in what
//!   comes next. A newline is unambiguous and is decided at once. Over-limit cuts wait for the
//!   lookahead of every boundary inside the limit, so the output does not depend on how the
//!   text was split into deltas.
//! - **Whitespace runs** longer than `max_chunk_chars + 1` are shortened to that length (a
//!   newline inside the excess is kept), so whitespace-only input cannot grow the pending text.
//! - **Never split**: decimals (`3.14`, `3,14`), thousands (`1,000`, `1.000`), times (`10:30`),
//!   Turkish ordinals (`3. sinif`, a digit, a period and a lowercase word), list markers at the
//!   start of a line, abbreviations (per language), dotted acronyms (`E.U.`, `N.Y.C.`), URLs and
//!   e-mail addresses, ellipses followed by a lowercase word. A bare number and period mid-line
//!   followed by a capital (`… now. 2. We left.`) is a sentence end.
//! - Chunks are trimmed; whitespace-only chunks are never emitted.
//!
//! [`Segmenter::push_spans`] and [`Segmenter::finish_spans`] are the same segmenter, reporting
//! for each chunk the byte range it was cut from in the accumulated stream ([`Span`]). A caller
//! that sends the stream's raw text (not the trimmed chunks) to TTS uses the ranges to cut it
//! into slices that concatenate to the stream byte for byte, so the text it has sent is always a
//! prefix of the message text.
//!
//! [`timed_chunks`] feeds a token stream with arrival times and reports when each chunk left the
//! segmenter, which is how the first-chunk latency is measured (`docs/LATENCY.md`).

use std::time::Duration;

/// Multiple of `max_chunk_chars` after which a token without whitespace is cut (module docs).
const HARD_CAP_FACTOR: usize = 4;

/// Language whose abbreviation list applies (see the module docs).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub enum Language {
    /// English: `Mr.`, `e.g.`, `U.S.`, `Inc.` and so on.
    #[default]
    En,
    /// Turkish: `Dr.`, `vb.`, `Sn.`, `Tel.`, `Şti.` and so on.
    Tr,
}

/// Segmenter settings.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SegmenterConfig {
    /// Language whose abbreviation list is used.
    pub language: Language,
    /// Words the first chunk must contain before it may end (default 3; 0 disables the rule).
    pub first_min_words: usize,
    /// Pending text longer than this many chars is cut at the last clause boundary or whitespace
    /// within the limit (default 250; values below 1 act as 1). See the module docs for the
    /// over-limit rules and the hard cap at 4 times this value.
    pub max_chunk_chars: usize,
}

impl SegmenterConfig {
    /// Defaults for `language`: `first_min_words` 3, `max_chunk_chars` 250.
    #[must_use]
    pub fn new(language: Language) -> Self {
        Self {
            language,
            first_min_words: 3,
            max_chunk_chars: 250,
        }
    }
}

impl Default for SegmenterConfig {
    fn default() -> Self {
        Self::new(Language::En)
    }
}

/// Incremental splitter of LLM text deltas into TTS chunks.
///
/// Not thread-safe by design: one segmenter per response stream. Pending text is held only until
/// its chunk is decided; see the module docs for the rules and the one-token lookahead.
#[derive(Debug)]
pub struct Segmenter {
    config: SegmenterConfig,
    /// Pending text. Leading whitespace is dropped at the end of each push.
    buf: Vec<char>,
    /// Whether the current stream already emitted a chunk (the first-chunk rule then ends).
    first_emitted: bool,
    /// Whether the pending text starts at the start of a line (or of the stream).
    at_line_start: bool,
    /// Whitespace chars of the current input run that were kept.
    ws_run: usize,
    /// Whether a newline beyond the kept whitespace of the current run was already appended.
    ws_newline_kept: bool,
    /// Byte offset in the accumulated stream (every delta pushed since the last
    /// [`Segmenter::reset`] or [`Segmenter::finish`], concatenated) of each char of `buf`.
    offsets: Vec<usize>,
    /// Bytes of the accumulated stream seen so far.
    stream_len: usize,
}

/// A chunk together with the byte range it was cut from.
///
/// `start..end` is the range of the (trimmed) chunk in the accumulated stream: every delta
/// pushed since the last [`Segmenter::reset`] or [`Segmenter::finish`], concatenated. The ranges
/// of one stream are increasing and do not overlap. `text` equals the stream's `start..end`
/// except where the segmenter dropped whitespace beyond its run cap (see the module docs); the
/// stream text between two ranges is whitespace (and, rarely, such dropped whitespace).
///
/// A caller that keeps the stream text can therefore emit the raw slice `prev_end..end` per
/// chunk (the chunk with the whitespace before it): the slices concatenate to the stream up to
/// the last chunk, byte for byte, which is what `PlayedText::push_sent` and `apply_truncation`
/// need to agree with the message text.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Span {
    /// The trimmed chunk, as [`Segmenter::push`] would have emitted it.
    pub text: String,
    /// Byte offset of the chunk's first char in the accumulated stream.
    pub start: usize,
    /// Byte offset just past the chunk's last char in the accumulated stream.
    pub end: usize,
}

impl Segmenter {
    /// Creates an empty segmenter for one stream.
    #[must_use]
    pub fn new(config: SegmenterConfig) -> Self {
        Self {
            config,
            buf: Vec::new(),
            first_emitted: false,
            at_line_start: true,
            ws_run: 0,
            ws_newline_kept: false,
            offsets: Vec::new(),
            stream_len: 0,
        }
    }

    /// Feeds one text delta and appends every chunk that became safe to speak to `out`.
    ///
    /// A boundary whose next non-space character has not arrived yet stays pending, so this may
    /// emit nothing even when the delta ends with a period. Pending text stays at or below
    /// `5 × max_chunk_chars + 1` chars after every push (see [`Segmenter::pending_chars`]).
    pub fn push(&mut self, delta: &str, out: &mut Vec<String>) {
        self.append(delta);
        self.drain(false, &mut |text, _, _| out.push(text));
    }

    /// Like [`Segmenter::push`], reporting for every chunk the byte range it was cut from in the
    /// accumulated stream (see [`Span`]). The chunks are exactly those `push` would emit.
    pub fn push_spans(&mut self, delta: &str, out: &mut Vec<Span>) {
        self.append(delta);
        self.drain(false, &mut |text, start, end| {
            out.push(Span { text, start, end });
        });
    }

    /// Ends the stream: emits everything still pending (decisions that waited for a next
    /// character are taken now), then resets the segmenter so the next stream starts with a
    /// first chunk again.
    pub fn finish(&mut self, out: &mut Vec<String>) {
        self.finish_with(&mut |text, _, _| out.push(text));
    }

    /// Like [`Segmenter::finish`], with the byte range of each chunk in the stream that ends
    /// here (see [`Span`]). The stream counts from zero again afterwards.
    pub fn finish_spans(&mut self, out: &mut Vec<Span>) {
        self.finish_with(&mut |text, start, end| {
            out.push(Span { text, start, end });
        });
    }

    /// Bytes of the accumulated stream so far: the length of the stream text a caller that
    /// keeps it should have. Zero after [`Segmenter::reset`] and [`Segmenter::finish`].
    #[must_use]
    pub fn stream_len(&self) -> usize {
        self.stream_len
    }

    fn append(&mut self, delta: &str) {
        let cap = self.ws_cap();
        for (at, c) in delta.char_indices() {
            let offset = self.stream_len.saturating_add(at);
            if !c.is_whitespace() {
                self.ws_run = 0;
                self.ws_newline_kept = false;
                self.buf.push(c);
                self.offsets.push(offset);
            } else if self.ws_run < cap {
                self.ws_run += 1;
                self.buf.push(c);
                self.offsets.push(offset);
            } else if c == '\n' && !self.ws_newline_kept {
                self.ws_newline_kept = true;
                self.buf.push('\n');
                self.offsets.push(offset);
            }
        }
        self.stream_len = self.stream_len.saturating_add(delta.len());
    }

    fn finish_with(&mut self, emit: &mut dyn FnMut(String, usize, usize)) {
        self.drain(true, emit);
        // What is left is not cut any more: one last chunk.
        let last = self.trimmed_span(0, self.buf.len()).map(|(first, last)| {
            let text: String = self.buf.get(first..last).unwrap_or(&[]).iter().collect();
            (text, self.byte_range(first, last))
        });
        self.reset();
        if let Some((text, (start, end))) = last {
            emit(text, start, end);
        }
    }

    /// The indices `(first, last_exclusive)` of the non-whitespace span of `buf[from..to]`, or
    /// `None` when that part is all whitespace.
    fn trimmed_span(&self, from: usize, to: usize) -> Option<(usize, usize)> {
        let view = self.buf.get(from..to)?;
        let first = view.iter().position(|c| !c.is_whitespace())?;
        let last = view.iter().rposition(|c| !c.is_whitespace())?;
        Some((from + first, from + last + 1))
    }

    /// Byte range in the stream of `buf[first..last]` (`first < last`).
    fn byte_range(&self, first: usize, last: usize) -> (usize, usize) {
        let start = self.offsets.get(first).copied().unwrap_or(0);
        let end = last
            .checked_sub(1)
            .and_then(|i| {
                let offset = self.offsets.get(i).copied()?;
                let c = self.buf.get(i)?;
                Some(offset.saturating_add(c.len_utf8()))
            })
            .unwrap_or(start);
        (start, end)
    }

    /// Discards pending text and starts a new stream, e.g. after a barge-in: the text was never
    /// spoken and must not be emitted, and the next chunk is a first chunk again.
    pub fn reset(&mut self) {
        self.buf.clear();
        self.offsets.clear();
        self.stream_len = 0;
        self.first_emitted = false;
        self.at_line_start = true;
        self.ws_run = 0;
        self.ws_newline_kept = false;
    }

    /// Number of chars held back (undecided or waiting for whitespace or a lookahead char). It
    /// stays at or below `5 × max_chunk_chars + 1` after every push, however the input looks.
    /// The normal bound is `4 × max_chunk_chars + 2`; the extra `max_chunk_chars + 1` is the
    /// whitespace tail a fallback cut waits on when the boundary is after an ambiguous period
    /// (`3.`, `…`, a domain) and its next char has not arrived yet.
    #[must_use]
    pub fn pending_chars(&self) -> usize {
        self.buf.len()
    }

    fn ws_cap(&self) -> usize {
        self.config.max_chunk_chars.max(1).saturating_add(1)
    }

    fn drain(&mut self, final_: bool, emit: &mut dyn FnMut(String, usize, usize)) {
        let mut base = skip_ws(&self.buf, 0);
        let mut at_line_start =
            line_start_after(self.at_line_start, self.buf.get(..base).unwrap_or(&[]));
        let mut first = !self.first_emitted;
        while let Some(cut) = next_cut(
            self.buf.get(base..).unwrap_or(&[]),
            final_,
            &self.config,
            first,
            at_line_start,
        ) {
            let view = self.buf.get(base..).unwrap_or(&[]);
            let next = skip_ws(view, cut.next.min(view.len()));
            if next == 0 {
                break;
            }
            let chunk = self.trimmed_span(base, base + cut.end.min(view.len()));
            at_line_start = line_start_after(at_line_start, view.get(..next).unwrap_or(&[]));
            if let Some((from, to)) = chunk {
                let text: String = self.buf.get(from..to).unwrap_or(&[]).iter().collect();
                let (start, end) = self.byte_range(from, to);
                emit(text, start, end);
                first = false;
            }
            base += next;
        }
        self.buf.drain(..base.min(self.buf.len()));
        self.offsets.drain(..base.min(self.offsets.len()));
        self.at_line_start = at_line_start;
        self.first_emitted = !first;
    }
}

/// One chunk the segmenter emitted, with the arrival time of the token that released it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TimedChunk {
    /// Index of the token whose arrival emitted the chunk (the last token for chunks from
    /// [`Segmenter::finish`]).
    pub token_index: usize,
    /// Arrival time of that token, as given by the caller.
    pub at: Duration,
    /// The trimmed chunk text.
    pub text: String,
    /// True when the chunk was emitted by [`Segmenter::finish`] (stream end), not by a token.
    pub from_finish: bool,
}

/// Feeds a token stream with arrival times through a [`Segmenter`] and reports each chunk with
/// the token that released it.
///
/// `tokens` holds `(text, arrival)` pairs in arrival order; the stream is finished at the arrival
/// time of the last token. Used to measure time-to-first-chunk without touching a real clock
/// (the caller supplies the timestamps, e.g. one token every 30 ms).
#[must_use]
pub fn timed_chunks(config: SegmenterConfig, tokens: &[(&str, Duration)]) -> Vec<TimedChunk> {
    let mut seg = Segmenter::new(config);
    let mut result = Vec::new();
    let mut texts = Vec::new();
    for (index, (text, at)) in tokens.iter().enumerate() {
        seg.push(text, &mut texts);
        for chunk in texts.drain(..) {
            result.push(TimedChunk {
                token_index: index,
                at: *at,
                text: chunk,
                from_finish: false,
            });
        }
    }
    let last_index = tokens.len().saturating_sub(1);
    let last_at = tokens.last().map_or(Duration::ZERO, |(_, at)| *at);
    seg.finish(&mut texts);
    for chunk in texts.drain(..) {
        result.push(TimedChunk {
            token_index: last_index,
            at: last_at,
            text: chunk,
            from_finish: true,
        });
    }
    result
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Kind {
    Sentence,
    Clause,
}

/// A boundary: the chunk is `buf[..end]`, the next chunk starts at `next` (after whitespace).
#[derive(Debug, Clone, Copy)]
struct Cut {
    end: usize,
    next: usize,
    kind: Kind,
}

/// What the scan needs besides the text itself.
struct Ctx {
    language: Language,
    final_: bool,
    at_line_start: bool,
}

/// Character at `i`, or `'\0'` past the end (never whitespace, punctuation or a letter).
fn ch(buf: &[char], i: usize) -> char {
    buf.get(i).copied().unwrap_or('\0')
}

fn skip_ws(buf: &[char], mut i: usize) -> usize {
    while i < buf.len() && ch(buf, i).is_whitespace() {
        i += 1;
    }
    i
}

/// Whether `buf[i]` starts a whitespace run that follows a non-space char.
fn is_ws_start(buf: &[char], i: usize) -> bool {
    i >= 1 && i < buf.len() && ch(buf, i).is_whitespace() && !ch(buf, i - 1).is_whitespace()
}

/// Whether `prev` (the line start state of `dropped`'s begin) carries over: `dropped` is the
/// text removed from the front of the pending buffer; the result says whether the new front is
/// at a line start.
fn line_start_after(prev: bool, dropped: &[char]) -> bool {
    let tail = dropped
        .iter()
        .rev()
        .take_while(|c| c.is_whitespace())
        .count();
    let tail_chars = dropped
        .get(dropped.len().saturating_sub(tail)..)
        .unwrap_or(&[]);
    let tail_has_newline = tail_chars.contains(&'\n');
    if tail == dropped.len() {
        prev || tail_has_newline
    } else {
        tail_has_newline
    }
}

fn is_term(c: char) -> bool {
    matches!(c, '.' | '!' | '?' | '…')
}

/// Closing quotes and brackets, and markdown emphasis or code markers, that may follow a
/// sentence end and then belong to the chunk before it.
fn is_closer(c: char) -> bool {
    matches!(
        c,
        '"' | '\'' | '”' | '’' | ')' | ']' | '}' | '»' | '›' | '*' | '_' | '`' | '~'
    )
}

fn is_clause_punct(c: char) -> bool {
    matches!(c, ',' | ';' | ':')
}

fn is_dash(c: char) -> bool {
    matches!(c, '-' | '–' | '—')
}

/// Whether a newline in the whitespace at `i` (after non-space text) ends the text before it.
///
/// A line that ends in a terminator (possibly followed by closers) is decided by the terminator
/// rules, so `1.` or `Dr.` at a line end is not cut here. A line that ends in a clause mark or a
/// dash does end its chunk, so a list `- eggs,` / `- milk,` is spoken item by item; such a line
/// must already hold a word, so a bare `-` bullet is not a chunk of its own.
fn newline_ends_chunk(buf: &[char], i: usize) -> bool {
    let mut p = i;
    while p > 0 && is_closer(ch(buf, p - 1)) {
        p -= 1;
    }
    if p == 0 {
        return true;
    }
    let c = ch(buf, p - 1);
    if is_term(c) {
        return false;
    }
    if is_clause_punct(c) || is_dash(c) {
        return buf
            .get(..i)
            .unwrap_or(&[])
            .iter()
            .any(|c| c.is_alphanumeric());
    }
    true
}

/// Case and dotless/dotted-i folding for abbreviation matching: `I`, `İ`, `ı` and `i` all map to
/// `i`, every other char to its lowercase form.
fn fold(c: char) -> char {
    match c {
        'I' | 'İ' | 'ı' | 'i' => 'i',
        _ => c.to_lowercase().next().unwrap_or(c),
    }
}

const EN_ABBREVIATIONS: &[&str] = &[
    "mr", "mrs", "ms", "dr", "prof", "st", "vs", "etc", "e.g", "i.e", "a.m", "p.m", "u.s", "u.k",
    "ph.d", "inc", "ltd", "corp", "jr", "sr", "jan", "feb", "aug", "sep", "sept", "oct", "nov",
    "dec", "approx", "dept", "fig", "vol", "gen", "sgt", "capt", "lt", "gov", "rep", "sen",
];

const TR_ABBREVIATIONS: &[&str] = &[
    "dr", "prof", "doc", "doç", "sn", "vb", "vs", "bsk", "tel", "cad", "sok", "mah", "apt", "ltd",
    "şti", "tic", "san", "örn", "bkz", "krş", "yrd", "müh",
];

fn abbreviations(language: Language) -> &'static [&'static str] {
    match language {
        Language::En => EN_ABBREVIATIONS,
        Language::Tr => TR_ABBREVIATIONS,
    }
}

/// Visits boundaries in text order, for candidates that end at or before `max`. `visit` returns
/// `false` to stop the scan. Boundaries that start after `max` end after it, so the scan stops
/// at `max` without missing any.
fn scan(buf: &[char], ctx: &Ctx, max: usize, mut visit: impl FnMut(Cut) -> bool) {
    let n = buf.len();
    let mut i = 0;
    while i < n && i <= max {
        let c = ch(buf, i);
        let prev = if i == 0 { ' ' } else { ch(buf, i - 1) };
        if is_term(c) && !is_term(prev) {
            // A run of terminators, then closing quotes/brackets/markers, then whitespace, then
            // the next character (the lookahead). The chunk keeps the closers.
            let mut j = i;
            while j < n && is_term(ch(buf, j)) {
                j += 1;
            }
            let mut k = j;
            while k < n && is_closer(ch(buf, k)) {
                k += 1;
            }
            let mut w = k;
            while w < n && ch(buf, w).is_whitespace() {
                w += 1;
            }
            if w > k && (w < n || ctx.final_) {
                let next = (w < n).then(|| ch(buf, w));
                if is_sentence_end(buf, i, j, next, ctx)
                    && !visit(Cut {
                        end: k,
                        next: w,
                        kind: Kind::Sentence,
                    })
                {
                    return;
                }
            }
            i = j;
            continue;
        }
        let after_is_space = ch(buf, i + 1).is_whitespace();
        let clause_mark = is_clause_punct(c) && after_is_space;
        let spaced_dash = is_dash(c) && prev.is_whitespace() && after_is_space;
        if clause_mark || spaced_dash {
            // Decided once the next non-space character is known.
            let w = skip_ws(buf, i + 1);
            if w < n
                && !visit(Cut {
                    end: i + 1,
                    next: w,
                    kind: Kind::Clause,
                })
            {
                return;
            }
        } else if c.is_whitespace() && !prev.is_whitespace() && i > 0 {
            // A newline ends the line's text (see `newline_ends_chunk`). Newlines are
            // unambiguous: no lookahead needed.
            let w = skip_ws(buf, i);
            let has_newline = buf.get(i..w).unwrap_or(&[]).contains(&'\n');
            if has_newline
                && newline_ends_chunk(buf, i)
                && !visit(Cut {
                    end: i,
                    next: w,
                    kind: Kind::Sentence,
                })
            {
                return;
            }
        }
        i += 1;
    }
}

/// Decides whether the terminator run `buf[start..end]` (followed by `next`, the first
/// non-space character after it, or `None` at the end of the text) ends a sentence.
fn is_sentence_end(buf: &[char], start: usize, end: usize, next: Option<char>, ctx: &Ctx) -> bool {
    if (start..end).any(|k| matches!(ch(buf, k), '?' | '!')) {
        return true;
    }
    if end.saturating_sub(start) >= 2 || ch(buf, start) == '…' {
        // Ellipsis: a pause; it ends the sentence only before something that does not continue
        // the thought (a lowercase word continues it).
        return !next.is_some_and(char::is_lowercase);
    }
    let (token_start, core) = token_before(buf, start);
    let folded: String = core.iter().map(|&c| fold(c)).collect();
    if folded == "no" {
        // "No. 5" is a number reference; a plain "no." ends a sentence.
        return !next.is_some_and(|c| c.is_ascii_digit());
    }
    if abbreviations(ctx.language).contains(&folded.as_str()) || is_acronym(&core) {
        return false;
    }
    let digits_only = !core.is_empty() && core.iter().all(char::is_ascii_digit);
    if digits_only {
        // "3. sinif" (ordinal) and "3. 14" (number) continue. A list marker ("1.") ends no
        // sentence at the start of a line; mid-line a number and period followed by a capital
        // is a sentence end.
        if next.is_some_and(|c| c.is_lowercase() || c.is_ascii_digit()) {
            return false;
        }
        if line_start_before(buf, token_start, ctx.at_line_start) {
            return false;
        }
    }
    let text: String = core.iter().collect();
    if text.contains("://") || text.contains('@') || is_domain(&core) {
        // URL, e-mail or dotted domain: a period that ends it is a sentence end only before a
        // capital letter or a digit.
        return next.is_none_or(|c| c.is_uppercase() || c.is_ascii_digit());
    }
    true
}

/// Dotted acronyms made of single letters (`E.U`, `N.Y.C`, `A.I`, `B.C`): their period is part
/// of the acronym, not a sentence end. The trailing period is not in `core`.
fn is_acronym(core: &[char]) -> bool {
    core.len() >= 3
        && core.len() % 2 == 1
        && core.iter().enumerate().all(|(k, &c)| {
            if k % 2 == 0 {
                c.is_alphabetic()
            } else {
                c == '.'
            }
        })
}

/// A dotted domain such as `example.com` or `ornek.com.tr`: an internal dot whose last label is
/// lowercase letters or digits (so `2.5` and `E.U` are not domains).
fn is_domain(core: &[char]) -> bool {
    let Some(dot) = core.iter().rposition(|&c| c == '.') else {
        return false;
    };
    let before = core.get(..dot).unwrap_or(&[]);
    let label = core.get(dot + 1..).unwrap_or(&[]);
    before.last().is_some_and(|c| c.is_alphanumeric())
        && !label.is_empty()
        && label.iter().all(|c| c.is_alphanumeric())
        && label.iter().any(|c| c.is_lowercase())
}

/// Whether the token starting at `token_start` begins a line: a newline lies in the whitespace
/// before it, or the token is the first of the pending text and the text begins a line.
fn line_start_before(buf: &[char], token_start: usize, at_view_start: bool) -> bool {
    let mut s = token_start;
    let mut newline = false;
    while s > 0 && ch(buf, s - 1).is_whitespace() {
        newline |= ch(buf, s - 1) == '\n';
        s -= 1;
    }
    if s == 0 {
        at_view_start || newline
    } else {
        newline
    }
}

/// The token ending right before `end` (up to whitespace), with leading non-alphanumerics
/// stripped (`(e.g` gives `e.g`). Returns the token's start index and its chars.
fn token_before(buf: &[char], end: usize) -> (usize, Vec<char>) {
    let mut start = end;
    while start > 0 && !ch(buf, start - 1).is_whitespace() {
        start -= 1;
    }
    let raw: Vec<char> = buf.get(start..end).unwrap_or(&[]).to_vec();
    let core: Vec<char> = raw
        .into_iter()
        .skip_while(|c| !c.is_alphanumeric())
        .collect();
    (start, core)
}

/// Whether `c` joins or modifies the char before it inside one grapheme cluster: ZWJ and ZWNJ,
/// variation selectors, combining marks (see [`is_combining_mark`]), skin tones and tags.
fn is_extender(c: char) -> bool {
    matches!(
        u32::from(c),
        0x200C | 0x200D | 0xFE00..=0xFE0F | 0x1F3FB..=0x1F3FF | 0xE0020..=0xE007F
    ) || is_combining_mark(c)
}

/// Combining marks of the common scripts: Latin and Cyrillic diacritics, Hebrew points, Arabic
/// harakat, Devanagari and Thai vowel signs and tone marks, kana voiced marks, and the Combining
/// Diacritical Marks blocks. A subset of Unicode `Mn`/`Mc`/`Me`, not all of them.
fn is_combining_mark(c: char) -> bool {
    matches!(
        u32::from(c),
        0x0300..=0x036F
            | 0x0483..=0x0489
            | 0x0591..=0x05BD
            | 0x05BF
            | 0x05C1..=0x05C2
            | 0x05C4..=0x05C5
            | 0x05C7
            | 0x0610..=0x061A
            | 0x064B..=0x065F
            | 0x0670
            | 0x06D6..=0x06DC
            | 0x06DF..=0x06E4
            | 0x06E7..=0x06E8
            | 0x06EA..=0x06ED
            | 0x0900..=0x0903
            | 0x093A..=0x093C
            | 0x093E..=0x094F
            | 0x0951..=0x0957
            | 0x0962..=0x0963
            | 0x0E31
            | 0x0E34..=0x0E3A
            | 0x0E47..=0x0E4E
            | 0x1AB0..=0x1AFF
            | 0x1DC0..=0x1DFF
            | 0x20D0..=0x20FF
            | 0x3099..=0x309A
            | 0xFE20..=0xFE2F
    )
}

fn is_regional_indicator(c: char) -> bool {
    matches!(u32::from(c), 0x1F1E6..=0x1F1FF)
}

/// Whether a cut before `buf[p]` (`p >= 1`) keeps grapheme clusters whole.
fn grapheme_safe(buf: &[char], p: usize) -> bool {
    let prev = ch(buf, p - 1);
    let cur = ch(buf, p);
    !(is_extender(cur)
        || prev == '\u{200D}'
        || (is_regional_indicator(prev) && is_regional_indicator(cur)))
}

/// Cut at the whitespace run starting at `i`.
fn whitespace_cut(buf: &[char], i: usize) -> Cut {
    Cut {
        end: i,
        next: skip_ws(buf, i),
        kind: Kind::Clause,
    }
}

/// Hard cap: cut at the nearest grapheme-safe char boundary in `2..=at`. When there is none (one
/// grapheme longer than the cap, e.g. a run of combining marks after a base char), the cut is
/// made at `at` itself: memory must stay bounded, and a cut at 1 would emit one mark per chunk.
fn hard_cut(buf: &[char], at: usize) -> Cut {
    let at = at.min(buf.len().saturating_sub(1)).max(1);
    let p = (2..=at)
        .rev()
        .find(|&p| grapheme_safe(buf, p))
        .unwrap_or(at);
    Cut {
        end: p,
        next: p,
        kind: Kind::Clause,
    }
}

/// The first boundary that may be emitted now, if any. `buf` starts at the pending text (no
/// leading whitespace). Every decision depends only on the chars that are present, so the same
/// text gives the same decisions however it arrived.
fn next_cut(
    buf: &[char],
    final_: bool,
    config: &SegmenterConfig,
    first: bool,
    at_line_start: bool,
) -> Option<Cut> {
    let max = config.max_chunk_chars.max(1);
    let ctx = Ctx {
        language: config.language,
        final_,
        at_line_start,
    };
    let mut words = WordCounter::default();
    let mut found: Option<Cut> = None;
    let mut last_clause: Option<Cut> = None;
    scan(buf, &ctx, max, |cut| {
        if cut.end > max {
            return false;
        }
        if first && words.to(buf, cut.end) < config.first_min_words {
            return true;
        }
        match cut.kind {
            Kind::Sentence => {
                found = Some(cut);
                false
            }
            Kind::Clause if first => {
                found = Some(cut);
                false
            }
            Kind::Clause => {
                last_clause = Some(cut);
                true
            }
        }
    });
    if found.is_some() {
        return found;
    }
    if buf.len() <= max {
        return None;
    }
    // Over the limit. Every boundary within the limit must have its lookahead before a fallback
    // is taken, i.e. a non-space char at or after `max` is pending (or the stream is over).
    let lookahead_known = final_ || buf.iter().skip(max).any(|c| !c.is_whitespace());
    if !lookahead_known {
        return None;
    }
    if last_clause.is_some() {
        return last_clause;
    }
    // The token is longer than the limit (or the fallback is a whitespace cut inside the limit):
    // see `fallback_whitespace` for which whitespace may be used.
    let hard = max.saturating_mul(HARD_CAP_FACTOR);
    let limit = hard.min(buf.len().saturating_sub(1));
    let within = fallback_whitespace(buf, &ctx, (1..=max).rev());
    match within {
        Fallback::Undecided => return None,
        Fallback::Found(i) => return Some(whitespace_cut(buf, i)),
        Fallback::NoCandidate => {}
    }
    let beyond = fallback_whitespace(buf, &ctx, max.saturating_add(1)..=limit);
    match beyond {
        Fallback::Undecided => return None,
        Fallback::Found(i) => return Some(whitespace_cut(buf, i)),
        Fallback::NoCandidate => {}
    }
    if buf.len() > hard {
        return Some(hard_cut(buf, hard));
    }
    None
}

/// Result of looking for a whitespace fallback cut.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Fallback {
    /// The whitespace start at this index is the cut.
    Found(usize),
    /// None of the candidates can be used.
    NoCandidate,
    /// The first candidate depends on a next char that has not arrived: wait for it.
    Undecided,
}

/// The first usable whitespace start among `candidates`, in order.
///
/// A fallback never cuts right after a terminator that `scan` declined (`Dr.`, `3.` before
/// `sinif`, `E.U.`): those tokens are not boundaries, even when the limit forces a cut. Whether a
/// terminator is declined can depend on the next non-space char, so a candidate whose next char
/// has not arrived stops the search ([`Fallback::Undecided`]) unless every possible next char
/// gives the same answer. That keeps the output independent of how the text was split. A
/// candidate after a plain word is usable at once.
fn fallback_whitespace(
    buf: &[char],
    ctx: &Ctx,
    candidates: impl Iterator<Item = usize>,
) -> Fallback {
    for i in candidates {
        if !is_ws_start(buf, i) {
            continue;
        }
        let Some((start, run_end)) = terminator_run_before(buf, i) else {
            return Fallback::Found(i);
        };
        let w = skip_ws(buf, i);
        let decided = if w < buf.len() || ctx.final_ {
            let next = (w < buf.len()).then(|| ch(buf, w));
            Some(is_sentence_end(buf, start, run_end, next, ctx))
        } else {
            // The next char is unknown. `is_sentence_end` only tells lowercase, uppercase, ASCII
            // digit and other chars (or the end of the text) apart: one representative of each.
            let outcomes = [None, Some('a'), Some('A'), Some('0'), Some('"')]
                .map(|next| is_sentence_end(buf, start, run_end, next, ctx));
            outcomes
                .iter()
                .all(|&o| o == outcomes[0])
                .then_some(outcomes[0])
        };
        match decided {
            None => return Fallback::Undecided,
            Some(true) => return Fallback::Found(i),
            Some(false) => {}
        }
    }
    Fallback::NoCandidate
}

/// The terminator run (`start..run_end`, without closers) that ends right before the whitespace
/// at `i`, if any.
fn terminator_run_before(buf: &[char], i: usize) -> Option<(usize, usize)> {
    let mut run_end = i;
    while run_end > 0 && is_closer(ch(buf, run_end - 1)) {
        run_end -= 1;
    }
    let mut start = run_end;
    while start > 0 && is_term(ch(buf, start - 1)) {
        start -= 1;
    }
    (start < run_end).then_some((start, run_end))
}

/// Incremental word count of a prefix. Callers pass non-decreasing ends.
#[derive(Debug, Default)]
struct WordCounter {
    pos: usize,
    words: usize,
    in_token: bool,
    has_alnum: bool,
}

impl WordCounter {
    /// Words in `buf[..end]`; a trailing token counts if it has an alphanumeric char.
    fn to(&mut self, buf: &[char], end: usize) -> usize {
        while self.pos < end {
            let c = ch(buf, self.pos);
            if c.is_whitespace() {
                if self.in_token && self.has_alnum {
                    self.words += 1;
                }
                self.in_token = false;
                self.has_alnum = false;
            } else {
                self.in_token = true;
                self.has_alnum |= c.is_alphanumeric();
            }
            self.pos += 1;
        }
        self.words + usize::from(self.in_token && self.has_alnum)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn word_counter_counts_alphanumeric_tokens_only() {
        let chars: Vec<char> = "Hi - 😊 there, 3.14 !".chars().collect();
        let mut counter = WordCounter::default();
        assert_eq!(counter.to(&chars, chars.len()), 3);
    }

    #[test]
    fn fold_merges_dotted_and_dotless_i() {
        assert_eq!(fold('I'), 'i');
        assert_eq!(fold('İ'), 'i');
        assert_eq!(fold('ı'), 'i');
        assert_eq!(fold('Ş'), 'ş');
    }

    #[test]
    fn acronyms_and_domains_are_told_apart() {
        let chars = |s: &str| s.chars().collect::<Vec<char>>();
        assert!(is_acronym(&chars("E.U")));
        assert!(is_acronym(&chars("N.Y.C")));
        assert!(!is_acronym(&chars("3.14")));
        assert!(is_domain(&chars("example.com")));
        assert!(!is_domain(&chars("E.U")));
        assert!(!is_domain(&chars("2.5")));
    }

    #[test]
    fn hard_cap_never_cuts_inside_a_grapheme() {
        let family: Vec<char> = "a👨\u{200d}👩\u{200d}👧b".chars().collect();
        // Index 2 is a ZWJ (cut before it would split the sequence); index 4 likewise.
        assert!(!grapheme_safe(&family, 2));
        assert!(!grapheme_safe(&family, 4));
        assert!(grapheme_safe(&family, 1));
        let flag: Vec<char> = "x🇹🇷".chars().collect();
        assert!(!grapheme_safe(&flag, 2));
    }

    #[test]
    fn combining_marks_of_common_scripts_are_not_cut_before() {
        // Latin, Cyrillic, Hebrew, Arabic, Devanagari, Thai and kana marks.
        for text in [
            "e\u{301}",
            "й\u{306}",
            "ב\u{5B8}",
            "ب\u{64E}",
            "क\u{93F}",
            "ก\u{E34}",
            "か\u{3099}",
        ] {
            let chars: Vec<char> = text.chars().collect();
            assert!(!grapheme_safe(&chars, 1), "cut before mark in {text:?}");
        }
        assert!(grapheme_safe(&"ab".chars().collect::<Vec<char>>(), 1));
    }

    #[test]
    fn hard_cut_falls_back_to_the_cap_not_to_position_one() {
        // One grapheme (base plus marks) with no safe boundary inside 2..=8: cut at the cap.
        let mut run: Vec<char> = vec!['x'];
        run.extend(std::iter::repeat_n('\u{301}', 10));
        assert_eq!(hard_cut(&run, 8).end, 8);
        // A safe boundary in 2..=at is still preferred.
        let mixed: Vec<char> = "ab👨\u{200d}👩".chars().collect();
        assert_eq!(hard_cut(&mixed, 5).end, 2);
    }

    /// The English abbreviations that must stay listed. Written out here, not read from the
    /// constant, so that a deleted or mistyped entry fails this test.
    const EN_EXPECTED: &[&str] = &[
        "mr", "mrs", "ms", "dr", "prof", "st", "vs", "etc", "e.g", "i.e", "a.m", "p.m", "u.s",
        "u.k", "ph.d", "inc", "ltd", "corp", "jr", "sr", "jan", "feb", "aug", "sep", "sept", "oct",
        "nov", "dec", "approx", "dept", "fig", "vol", "gen", "sgt", "capt", "lt", "gov", "rep",
        "sen",
    ];

    /// The Turkish abbreviations that must stay listed (see [`EN_EXPECTED`]).
    const TR_EXPECTED: &[&str] = &[
        "dr", "prof", "doc", "doç", "sn", "vb", "vs", "bsk", "tel", "cad", "sok", "mah", "apt",
        "ltd", "şti", "tic", "san", "örn", "bkz", "krş", "yrd", "müh",
    ];

    /// Every expected abbreviation, followed by a capitalised word and a sentence, must not end a
    /// sentence at its period when it is not the first chunk (where the first-chunk word rule
    /// could hide a missing entry). Acronym-shaped entries (`u.s`, `e.g`) are also covered by the
    /// acronym rule, so this checks behaviour as well as list membership.
    #[test]
    fn every_listed_abbreviation_is_recognised_in_both_languages() {
        assert_eq!(EN_ABBREVIATIONS, EN_EXPECTED, "English list changed");
        assert_eq!(TR_ABBREVIATIONS, TR_EXPECTED, "Turkish list changed");
        for (language, list) in [(Language::En, EN_EXPECTED), (Language::Tr, TR_EXPECTED)] {
            for abbreviation in list {
                let input = format!("Alpha beta gamma {abbreviation}. Smith came here. Yes.");
                let config = SegmenterConfig {
                    language,
                    ..SegmenterConfig::default()
                };
                let mut seg = Segmenter::new(config);
                let mut out = Vec::new();
                seg.push(&input, &mut out);
                seg.finish(&mut out);
                assert_eq!(
                    out,
                    [
                        format!("Alpha beta gamma {abbreviation}. Smith came here."),
                        "Yes.".to_owned()
                    ],
                    "abbreviation {abbreviation:?} ({language:?})"
                );
            }
        }
    }
}
