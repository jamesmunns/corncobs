//! Support for framing data into a resynchronizable stream using Consistent
//! Overhead Byte Stuffing (COBS). This implementation is deliberately fairly
//! simple and corny.
//!
//! # COBS in general
//!
//! COBS lets us take an arbitrary blob of bytes and turn it into a slightly
//! longer blob that doesn't contain a certain byte, except as a terminator at
//! the very end. This makes it useful for framing packets on serial lines, or
//! other contexts where you'd like to be able to detect frame or record
//! boundaries.
//!
//! The "slightly longer" part is important. COBS guarantees an upper bound to
//! the size of the encoded output: the original length, plus two bytes, plus
//! one byte per 254 input bytes. Other alternatives like SLIP framing can have
//! worst-case overhead up to 100%, which means to meet your application's
//! requirements, you may need to reserve up to 50% of bandwidth at all times.
//!
//! Delightfully, the overhead of COBS _decreases_ if the data you're sending
//! contains the termination byte. This is the opposite of SLIP, and can cause
//! the average-case overhead to be very nearly zero.
//!
//! COBS is an example of a _resynchronizable_ framing protocol. An observer can
//! drop into the stream at any time, without context, and find the next message
//! boundary after losing at most one message to truncation. While this
//! description has been focused on communication channels like serial lines,
//! this resynchronization property means COBS can _also_ be valuable for data
//! stored on disk, where it might be truncated, or in a circular buffer.
//!
//! # `corncobs` specifically
//!
//! `corncobs` implements COBS for the common case where the termination byte is
//! zero. (COBS can technically be implemented for _any_ termination byte value,
//! though it's rare to use a value other than zero.) Messages on a stream are
//! each terminated by a single zero byte, and COBS lets us avoid having other
//! distracting zero bytes in the middle of a message.
//!
//! `corncobs` is specifically designed for (and tested on!) `no_std` embedded
//! platforms without heaps.
//!
//! `corncobs` can be used in several different ways, each with different costs
//! and benefits.
//!
//! - Encoding
//!   - `encode_buf`: from one slice to another; efficient, but requires 2x the
//!      available RAM.
//!   - `encode_iter`: incremental, using an iterator; somewhat slower, but
//!     requires no additional memory. (This can be useful in a serial interrupt
//!     handler.)
//! - Decoding
//!   - `decode_buf`: from one slice to another; efficient, but requires 2x the
//!     available RAM.
//!   - `decode_in_place`: in-place in a slice; nearly as efficient, but
//!     overwrites incoming data.

#![cfg_attr(not(feature = "std"), no_std)]

// So far, the implementation is performant without the use of `unsafe`. To
// ensure that I think before breaking this property down the road, I'm
// currently configuring the compiler to reject `unsafe`. This is not a promise
// or a religious decision and might get changed in the future; merely scanning
// for the presence of `unsafe` is neither necessary nor sufficient for auditing
// crates you depend on, including this one.
#![forbid(unsafe_code)]

/// The termination byte used by `corncobs`. Yes, it's a bit silly to have this
/// as a constant -- but the implementation is careful to use this named
/// constant whenever it is talking about the termination byte, for clarity.
///
/// The value of this (`0`) is assumed by the implementation and can't easily be
/// changed.
pub const ZERO: u8 = 0;

/// Longest run of unchanged bytes that can be encoded using COBS.
///
/// Changing this will decrease encoding efficiency and break compatibility with
/// other COBS implementations, so, don't do that.
const MAX_RUN: usize = 254;

/// Returns the largest possible encoded size for an input message of `raw_len`
/// bytes, considering overhead.
///
/// This is a `const fn` so that you can use it to size arrays:
///
/// ```
/// const MSG_SIZE: usize = 254;
/// // Worst-case input message: no zeroes to exploit.
/// let mut msg = [0xFF; MSG_SIZE];
/// // This will still be enough space!
/// let mut encoded = [0; corncobs::max_encoded_len(MSG_SIZE)];
///
/// let len = corncobs::encode_buf(&msg, &mut encoded);
/// assert_eq!(len, encoded.len());
/// ```
pub const fn max_encoded_len(raw_len: usize) -> usize {
    let overhead = if raw_len == 0 {
        // In the special case of an empty message, we wind up generating one
        // byte of overhead.
        1
    } else {
        (raw_len + 253) / 254
    };
    // +1 for terminator byte.
    raw_len + overhead + 1
}

/// Encodes the message `bytes` into the buffer `output`. Returns the number of
/// bytes used in `output`, which also happens to be the index of the first zero
/// byte.
///
/// Bytes in `output` after the part that gets used are left unchanged.
///
/// `output` must be large enough to receive the encoded form, which is
/// `max_encoded_len(bytes.len())` worst-case.
///
/// # Panics
///
/// If `output` is too small to contain the encoded form of `input`.
pub fn encode_buf(bytes: &[u8], mut output: &mut [u8]) -> usize {
    // We'll panic if the precondition is violated regardless, but this makes
    // the error a bit easier to spot in tests:
    debug_assert!(output.len() >= max_encoded_len(bytes.len()));

    // Capture the original size of the output, because we're going to shorten
    // it as we write bytes.
    let orig_size = output.len();

    // The encoding process can be described in terms of "runs" of non-zero
    // bytes in the input data. We process each run individually.
    //
    // Currently, the scanning-for-zeros loop here is the hottest part of the
    // encode profile.
    for mut run in bytes.split(|&b| b == ZERO) {
        // We can only encode a run of up to `MAX_RUN` bytes in COBS. This may
        // require us to split `run` into multiple output chunks -- in the
        // extreme case, if the input contains no zeroes, we'll process all of
        // it here.
        loop {
            let chunk_len = usize::min(run.len(), MAX_RUN);
            let (chunk, new_output) = output.split_at_mut(chunk_len + 1);
            let (run_prefix, new_run) = run.split_at(chunk_len);
            chunk[1..].copy_from_slice(run_prefix);
            chunk[0] = encode_len(chunk_len);

            output = new_output;
            run = new_run;

            // We test this condition here, rather than as a `while` loop,
            // because we want to process empty runs once.
            if run.is_empty() {
                break;
            }
        }
    }
    // We've been shortening the output as we go by lopping off prefixes, so our
    // terminating byte goes at the new start:
    output[0] = 0;
    orig_size - (output.len() - 1)
}

/// Encodes `bytes` into the vector `output`. This is a convenience for cases
/// where you have `std` available.
#[cfg(feature = "std")]
pub fn encode(bytes: &[u8], output: &mut Vec<u8>) {
    // Big computers with `std` have effectively unlimited memory, so, go ahead
    // and resize that vector to the maximum we might need:
    let offset = output.len();
    output.resize(offset + max_encoded_len(bytes.len()), 0);
    // Now just treat it as a slice.
    let actual_len = encode_buf(bytes, &mut output[offset..]);
    output.truncate(offset + actual_len);
}

/// Encoding a len (between `0` and `MAX_RUN` inclusive) into a byte such that
/// we avoid `ZERO`.
#[inline(always)]
fn encode_len(len: usize) -> u8 {
    // This assert is intended to catch mistakes while hacking on the internals
    // of corncobs.
    debug_assert!(len <= MAX_RUN);
    // This function is private and all paths through the code are reasonably
    // well tested, so we're pretty sure the assert above holds even in release
    // builds. As a result, explicitly opt out of overflow checks on this
    // addition.
    //
    // We're doing the addition on `usize` to ensure we don't generate
    // additional zero extend instructions.
    len.wrapping_add(1) as u8
}

/// Decodes `bytes` into a vector.
///
/// This is a convenience for cases where you have `std` available. Its behavior
/// is otherwise identical to `decode_buf`.
#[cfg(feature = "std")]
pub fn decode(bytes: &[u8], output: &mut Vec<u8>) -> Result<(), CobsError> {
    let offset = output.len();
    output.resize(offset + bytes.len(), 0);
    let actual_len = decode_buf(bytes, &mut output[offset..])?;
    output.truncate(offset + actual_len);
    Ok(())
}

/// Decodes input from `bytes` into `output` starting at index 0. Returns the
/// number of bytes used in `output`.
///
/// # No validation
///
/// For performance, this function does _not_ validate that the input doesn't
/// contain zero. This was a conscious choice made for the following reasons.
///
/// First: By not inspecting every byte, we can use `copy_from_slice` to move
/// chunks of decoded data; this winds up calling into the compiler's `memcpy`
/// (non-overlapping) intrinsic and will use the largest chunk size available on
/// the platform -- on Intel this means SSE/AVX, for instance. This also
/// eliminates a set of tests and conditional branches from this hot loop.
///
/// Second: COBS in general does not guarantee integrity, so you're going to
/// wind up running an integrity check (e.g. CRC validation) over the decode
/// result _anyway._ Given that obligatory second pass over the data, adding a
/// third pass to look for zeroes would be wasted effort.
///
/// Third: despite being fast, the algorithm used here will fail in very
/// predictable ways if the input isn't valid COBS:
///
/// 1. It will find a zero too early and return a very short decoded result.
///    This will fail your next-level integrity check and be rejected.
/// 2. It will continue following run-length bytes until it hits the end of
///    input, and will return `Err(CobsError::Truncated)`.
///
/// Even without the zero check, it should not be possible to get decoding to
/// `panic!` on arbitrary invalid input.
///
/// # Panics
///
/// If `output` is not long enough to receive the decoded output. To be safe,
/// `output` must be at least `max_encoded_len(bytes.len())`.
pub fn decode_buf(mut bytes: &[u8], mut output: &mut [u8]) -> Result<usize, CobsError> {
    let orig_len = output.len();

    let mut trailing_zero = false;
    // This while-loop is equivalent to `for b in bytes` except that it lets us
    // _also_ consume bytes inside the body, which we totally do.
    while let Some((&head, rest)) = bytes.split_first() {
        // Detect message terminator.
        let n = if let Some(n) = decode_len(head) {
            n
        } else {
            let decoded_len = orig_len - output.len();
            return Ok(decoded_len);
        };
        // If we're not at the end of the message, and our last run was less
        // than MAX_RUN bytes, we need to insert a zero.
        if core::mem::replace(&mut trailing_zero, false) {
            let (z, new_output) = output.split_at_mut(1);
            z[0] = ZERO;
            output = new_output;
        }
        // Split `rest` into our run and, well, the rest.
        if rest.len() < n {
            return Err(CobsError::Truncated);
        }
        let (block, rest) = rest.split_at(n);

        // Blit that block!
        let (block_out, new_output) = output.split_at_mut(block.len());
        block_out.copy_from_slice(block);
        output = new_output;

        // Record whether this run was shorter than the max. Runs shorter than
        // the max in the middle of a message are always ended by zero, which we
        // need to insert in the output. However, a shorter-than-max run at the
        // very _end_ is not terminated by zero, and we handle it above.
        trailing_zero = n != MAX_RUN;

        // Discard processed input.
        bytes = rest;
    }

    // If we got here, it's because we ran all the way through `bytes` without
    // finding the terminating ZERO.
    Err(CobsError::Truncated)
}

/// Errors that can occur while decoding.
#[derive(Copy, Clone, Debug)]
pub enum CobsError {
    /// The input ended without completing the last run or without the trailing
    /// zero byte, suggesting that part of it is missing. (This can also occur
    /// spuriously if you pick up in the middle of a stream without finding the
    /// first zero.)
    Truncated,
}

impl core::fmt::Display for CobsError {
    fn fmt(&self, f: &mut core::fmt::Formatter) -> core::fmt::Result {
        match self {
            Self::Truncated => f.write_str("input truncated"),
        }
    }
}

#[cfg(feature = "std")]
impl std::error::Error for CobsError {}

/// Decodes a length-or-terminator byte. If the byte is `ZERO`, returns `None`.
/// Otherwise returns the length of the run encoded by the byte.
#[inline(always)]
fn decode_len(code: u8) -> Option<usize> {
    usize::from(code).checked_sub(1)
}

/// Decodes an encoded message, in-place. This is useful when you're short on
/// memory. Since the decoded form of a COBS frame is always shorter than the
/// encoded form, `bytes` is guaranteed to be long enough.
///
/// The decoded message is deposited into `bytes` starting at index 0, and
/// `decode_in_place` returns the number of decoded bytes.
///
/// If you've got memory to spare, `decode_buf` is often somewhat faster --
/// `decode_in_place` takes between 1x and 3x the time in benchmarks. You may
/// also prefer to use `decode_buf` if you can't overwrite the incoming data,
/// for whatever reason.
///
/// # No validation
///
/// This does not check for invalid zeroes in the input, for performance
/// reasons. If you're curious, please see the detailed justification on
/// [`decode_buf`].
pub fn decode_in_place(bytes: &mut [u8]) -> Result<usize, CobsError> {
    let mut inpos = 0;
    let mut outpos = 0;
    let mut extra_zero = false;
    while inpos < bytes.len() {
        let head = bytes[inpos];
        let n = if let Some(n) = decode_len(head) {
            n
        } else {
            break;
        };
        if bytes.len() < inpos + 1 + n {
            return Err(CobsError::Truncated);
        }
        bytes.copy_within(inpos + 1..inpos + 1 + n, outpos);
        inpos += 1 + n;
        outpos += n;
        extra_zero = n != MAX_RUN;
        if extra_zero {
            bytes[outpos] = 0;
            outpos += 1;
        }
    }
    Ok(if extra_zero {
        outpos - 1
    } else {
        outpos
    })
}

/// State for incremental encoding.
#[derive(Copy, Clone, Debug)]
enum State<'a> {
    /// We are at a run boundary and need to determine the size of the next run
    /// and emit an overhead byte.
    ///
    /// From this state we will always emit at least two bytes: an overhead byte
    /// and a terminator.
    ///
    /// If the next run contains only 0, we'll drop it and transition back to
    /// `Begin`.
    ///
    /// Otherwise, we'll transition to `Run` to send the bytes.
    ///
    /// If the data is empty we'll transition to `End`.
    Begin(&'a [u8]),
    /// We are in a non-empty run. We need to emit a literal byte, and then determine
    /// our next state based on whether the first slice is empty.
    ///
    /// If the first slice is empty, and the second slice is `None`, we'll
    /// transition to `End`.
    ///
    /// If the first slice is empty, and the second slice is `Some`, we'll
    /// transition to `Begin`.
    ///
    /// Otherwise we'll remain in `Run`, moving the first byte out of the first
    /// slice.
    Run(u8, &'a [u8], Option<&'a [u8]>),
    /// We have used all the data bytes and just need to emit a terminating
    /// zero.
    ///
    /// This state will always emit exactly one byte.
    End,
}

impl<'a> State<'a> {
    pub fn next(self) -> (u8, Option<Self>) {
        match self {
            State::Begin(bytes) => {
                let (run, rest) = take_run(bytes);
                let b = encode_len(run.len());
                (b, Some(Self::next_run_state(run, rest)))
            }
            State::Run(b, run, rest) => {
                (b, Some(Self::next_run_state(run, rest)))
            }
            State::End => (0, None),
        }
    }

    fn next_run_state(run: &'a [u8], rest: Option<&'a [u8]>) -> Self {
        if let Some((&b, run)) = run.split_first() {
            // There's data in the run, we must drain it before starting
            // a new one.
            State::Run(b, run, rest)
        } else {
            State::new_run_state(rest)
        }
    }

    fn new_run_state(rest: Option<&'a [u8]>) -> Self {
        if let Some(rest) = rest {
            State::Begin(rest)
        } else {
            State::End
        }
    }
}

/// Takes a run off the front of `bytes`. The run will be between 0 and
/// `MAX_RUN` bytes, inclusive, and will not include any `ZERO` bytes.
///
/// If the run is empty, it means the next byte in `bytes` was `ZERO`.
///
/// Returns `(run, rest)`, where `rest` is...
///
/// - `None`, if this run consumed the entire slice.
/// - `Some(stuff)`, if after this run there is still data to process.
///
/// Note that `stuff` may be empty, if `bytes` ends in a `ZERO`. It is still
/// important to process `stuff` in that case.
fn take_run(bytes: &[u8]) -> (&[u8], Option<&[u8]>) {
    // The run will be no longer than
    // - All the bytes, or
    // - The fixed MAX_RUN constant.
    let max_len = usize::min(bytes.len(), MAX_RUN);
    // It may be shorter than that if there's a zero. Scan the prefix for a zero
    // and truncate if found.
    let run_len = bytes.iter()
        .take(max_len)
        .position(|&b| b == ZERO)
        .unwrap_or(max_len);

    let (run, rest) = bytes.split_at(run_len);
    let rest = if rest.is_empty() {
        None
    } else if run_len == MAX_RUN {
        // Run does not imply a zero, don't omit one from the output if present.
        Some(rest)
    } else {
        debug_assert_eq!(rest[0], 0);
        // Drop the zero.
        Some(&rest[1..])
    };
    (run, rest)
}

/// Encodes `bytes` into COBS form, yielding individual encoded bytes through an
/// iterator.
///
/// This is quite a bit slower than memory-to-memory encoding (e.g.
/// `encode_buf`) because it can't move whole blocks of non-zero bytes at a
/// time -- about 35-40x slower in benchmarks. However, if your throughput is
/// restricted by the speed of a link that gets fed one byte a time, such as a
/// serial peripheral, this can encode messages with no additional memory.
pub fn encode_iter<'a>(bytes: &'a [u8]) -> impl Iterator<Item = u8> + 'a {
    let mut state = Some(State::Begin(bytes));
    core::iter::from_fn(move || {
        let s = state?;
        let (b, s2) = s.next();
        state = s2;
        Some(b)
    })
}

// Tests for private bits; test fixtures require std, unfortunately, so you have
// to run these explicitly with `cargo test --features std`. Most of the API
// tests are broken out into an integration test.
#[cfg(all(test, feature = "std"))]
mod tests {
    use super::*;

    #[test]
    fn take_run_empty() {
        assert_eq!(take_run(&[]), (&[][..], None));
    }

    #[test]
    fn take_run_zero() {
        assert_eq!(take_run(&[0]), (&[][..], Some(&[][..])));
    }

    #[test]
    fn take_run_one() {
        assert_eq!(take_run(&[1]), (&[1][..], None));
    }
}
