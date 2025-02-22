use super::auxility::ByteSliceExt;
use super::auxility::UncheckedSliceExt;
use super::bits::Bits;
use super::huffman::HuffmanDecoder;
use super::huffman::HuffmanEncoder;
use super::matchfinder::Bucket;
use super::matchfinder::BucketMatcher;
use super::matchfinder::MatchResult;
use super::mtf::MTFCoder;

const LZ_ROID_ENCODING_ARRAY: [(u8, u8, u16); super::LZ_MF_BUCKET_ITEM_SIZE] = include!(
    concat!(env!("OUT_DIR"), "/", "LZ_ROID_ENCODING_ARRAY.txt"));
const LZ_ROID_DECODING_ARRAY: [(u16, u8); super::LZ_ROID_SIZE] = include!(
    concat!(env!("OUT_DIR"), "/", "LZ_ROID_DECODING_ARRAY.txt"));

const WORD_SYMBOL: u16 = super::MTF_NUM_SYMBOLS as u16 - 1;

pub struct LZCfg {
    pub match_depth: usize,
    pub lazy_match_depth1: usize,
    pub lazy_match_depth2: usize,
}

struct LZContext {
    buckets:       Vec<Bucket>,
    mtfs:          Vec<MTFCoder>,
    words:         Vec<(u8, u8)>,
    after_literal: bool,

} impl LZContext {
    pub fn new() -> LZContext {
        return LZContext {
            buckets:       (0..256).map(|_| Bucket::new()).collect(),
            mtfs:          vec![],
            words:         vec![(0, 0); 32768],
            after_literal: true,
        };
    }
}

pub struct LZEncoder {
    ctx: LZContext,
    bucket_matchers: Vec<BucketMatcher>,

} impl LZEncoder {
    pub fn new() -> LZEncoder {
        return LZEncoder {
            ctx: LZContext::new(),
            bucket_matchers: (0..256).map(|_| BucketMatcher::new()).collect(),
        };
    }

    pub fn forward(&mut self, forward_len: usize) {
        for i in 0..self.bucket_matchers.len() {
            self.ctx.buckets[i].forward(forward_len);
            self.bucket_matchers[i].forward(&self.ctx.buckets[i]);
        }
    }

    pub unsafe fn encode(&mut self, cfg: &LZCfg, sbuf: &[u8], tbuf: &mut [u8], spos: usize) -> (usize, usize) {
        enum MatchItem {
            Match  {symbol: u16, mtf_context: u16, mtf_unlikely: u8, robitlen: u8, robits: u16, encoded_match_len: u8},
            Symbol {symbol: u16, mtf_context: u16, mtf_unlikely: u8},
        }
        let mut bits: Bits = Default::default();
        let mut spos = spos;
        let mut tpos = 0;
        let mut match_items = Vec::with_capacity(super::LZ_CHUNK_SIZE);

        let sc  = |pos| (sbuf.nc()[pos]);
        let sw  = |pos| (sbuf.nc()[pos - 1], sbuf.nc()[pos]);
        let shc = |pos| sc(pos) as usize & 0x7f | (u8::is_ascii_alphanumeric(&sbuf.nc()[pos - 1]) as usize) << 7;
        let shw = |pos| sc(pos) as usize & 0x7f | shc(pos - 1) << 7;

        // start Lempel-Ziv encoding
        while spos < sbuf.len() && match_items.len() < match_items.capacity() {
            let last_word_expected = self.ctx.words.nc()[shw(spos - 1)];
            let mtf_context = (self.ctx.after_literal as u16) << 8 | shc(spos - 1) as u16;
            let mtf_unlikely = last_word_expected.0;

            // encode as match
            let mut lazy_match_rets = (false, false);
            let match_result = self.bucket_matchers.nc()[shc(spos - 1)].find_match(
                &self.ctx.buckets.nc()[shc(spos - 1)],
                sbuf,
                spos,
                cfg.match_depth);

            if let MatchResult::Matched {reduced_offset, match_len, match_len_expected, match_len_min} = match_result {
                let (roid, robitlen, robits) = LZ_ROID_ENCODING_ARRAY.nc()[reduced_offset as usize];
                let lazy_len1 = match_len + 1 + (robitlen < 8) as usize;
                let lazy_len2 = lazy_len1 - (self.ctx.words.nc()[shw(spos - 1)] == sw(spos + 1)) as usize;

                lazy_match_rets.0 = match_len < super::LZ_MATCH_MAX_LEN / 2 &&
                    self.bucket_matchers.nc()[shc(spos + 0)].has_lazy_match(
                        &self.ctx.buckets.nc()[shc(spos + 0)],
                        sbuf,
                        spos + 1,
                        lazy_len1,
                        cfg.lazy_match_depth1);
                lazy_match_rets.1 = match_len < super::LZ_MATCH_MAX_LEN / 2 && !lazy_match_rets.0 &&
                    self.bucket_matchers.nc()[shc(spos + 1)].has_lazy_match(
                        &self.ctx.buckets.nc()[shc(spos + 1)],
                        sbuf,
                        spos + 2,
                        lazy_len2,
                        cfg.lazy_match_depth2);

                if !lazy_match_rets.0 && !lazy_match_rets.1 {
                    let encoded_match_len = match match_len.cmp(&match_len_expected) {
                        std::cmp::Ordering::Greater => match_len - match_len_min,
                        std::cmp::Ordering::Less    => match_len - match_len_min + 1,
                        std::cmp::Ordering::Equal   => 0,
                    } as u8;
                    let lenid = std::cmp::min(super::LZ_LENID_SIZE as u8 - 1, encoded_match_len);
                    let encoded_roid_lenid = 256 + roid as u16 * super::LZ_LENID_SIZE as u16 + lenid as u16;
                    match_items.push(MatchItem::Match {
                        symbol: encoded_roid_lenid,
                        mtf_context, mtf_unlikely, robitlen, robits, encoded_match_len,
                    });

                    self.ctx.buckets.nc_mut()[shc(spos - 1)].update(spos, reduced_offset, match_len);
                    self.bucket_matchers.nc_mut()[shc(spos - 1)].update(&self.ctx.buckets.nc()[shc(spos - 1)], sbuf, spos);
                    spos += match_len;
                    self.ctx.after_literal = false;
                    self.ctx.words.nc_mut()[shw(spos - 3)] = sw(spos - 1);
                    continue;
                }
            }
            self.ctx.buckets.nc_mut()[shc(spos - 1)].update(spos, 0, 0);
            self.bucket_matchers.nc_mut()[shc(spos - 1)].update(&self.ctx.buckets.nc()[shc(spos - 1)], sbuf, spos);

            // encode as symbol
            if spos + 1 < sbuf.len() && !lazy_match_rets.0 && last_word_expected == sw(spos + 1) {
                match_items.push(MatchItem::Symbol {symbol: WORD_SYMBOL, mtf_context, mtf_unlikely});
                spos += 2;
                self.ctx.after_literal = false;
            } else {
                match_items.push(MatchItem::Symbol {symbol: sc(spos) as u16, mtf_context, mtf_unlikely});
                spos += 1;
                self.ctx.after_literal = true;
                self.ctx.words.nc_mut()[shw(spos - 3)] = sw(spos - 1);
            }
        }

        // init mtf array
        if self.ctx.mtfs.is_empty() {
            let mut symbol_counts = [0; super::MTF_NUM_SYMBOLS];
            match_items.iter().for_each(|match_item| match match_item {
                &MatchItem::Match {symbol, ..} | &MatchItem::Symbol {symbol, ..} => {
                    symbol_counts.nc_mut()[symbol as usize] += 1;
                }
            });
            let mut vs = (0 .. super::MTF_NUM_SYMBOLS as u16).collect::<Vec<_>>();
            vs.sort_by_key(|v| -symbol_counts.nc()[*v as usize]);
            vs.iter().for_each(|v| tbuf.write_forward(&mut tpos, v.to_le()));
            self.ctx.mtfs = vec![MTFCoder::from_vs(&vs); 512];
        }

        // encode match_items_len
        bits.put(32, std::cmp::min(spos, sbuf.len()) as u64);
        bits.put(32, match_items.len() as u64);
        bits.save_u32(tbuf, &mut tpos);
        bits.save_u32(tbuf, &mut tpos);

        // start Huffman encoding
        let mut huff_weights1 = [0u32; super::MTF_NUM_SYMBOLS];
        let mut huff_weights2 = [0u32; super::LZ_MATCH_MAX_LEN];
        match_items.iter_mut().for_each(|match_item| match match_item {
            &mut MatchItem::Match  {ref mut symbol, mtf_context, mtf_unlikely, encoded_match_len, ..} => {
                *symbol = self.ctx.mtfs.nc_mut()[mtf_context as usize].encode(*symbol, mtf_unlikely as u16);
                huff_weights1.nc_mut()[*symbol as usize] += 1;
                huff_weights2.nc_mut()[encoded_match_len as usize] +=
                    (encoded_match_len as usize >= super::LZ_LENID_SIZE - 1) as u32;
            }
            &mut MatchItem::Symbol {ref mut symbol, mtf_context, mtf_unlikely, ..} => {
                *symbol = self.ctx.mtfs.nc_mut()[mtf_context as usize].encode(*symbol, mtf_unlikely as u16);
                huff_weights1.nc_mut()[*symbol as usize] += 1;
            }
        });

        let huff_encoder1 = HuffmanEncoder::new(&huff_weights1, 15, tbuf, &mut tpos);
        let huff_encoder2 = HuffmanEncoder::new(&huff_weights2, 15, tbuf, &mut tpos);
        match_items.iter().for_each(|match_item| match match_item {
            &MatchItem::Symbol {symbol, ..} => {
                huff_encoder1.encode_to_bits(symbol, &mut bits);
                bits.save_u32(tbuf, &mut tpos);
            },
            &MatchItem::Match {symbol, robitlen, robits, encoded_match_len, ..} => {
                huff_encoder1.encode_to_bits(symbol, &mut bits);
                bits.put(robitlen, robits as u64);
                bits.save_u32(tbuf, &mut tpos);
                if encoded_match_len as usize >= super::LZ_LENID_SIZE - 1 {
                    huff_encoder2.encode_to_bits(encoded_match_len as u16, &mut bits);
                    bits.save_u32(tbuf, &mut tpos);
                }
            }
        });
        bits.save_all(tbuf, &mut tpos);
        return (spos, tpos);
    }
}

pub struct LZDecoder {
    ctx: LZContext,

} impl LZDecoder {
    pub fn new() -> LZDecoder {
        return LZDecoder {
            ctx: LZContext::new(),
        };
    }

    pub fn forward(&mut self, forward_len: usize) {
        self.ctx.buckets.iter_mut().for_each(|bucket| bucket.forward(forward_len));
    }

    pub unsafe fn decode(&mut self, tbuf: &[u8], sbuf: &mut [u8], spos: usize) -> Result<(usize, usize), ()> {
        let mut bits: Bits = Default::default();
        let mut spos = spos;
        let mut tpos = 0;

        let sc  = |pos| (sbuf.nc()[pos as usize]);
        let sw  = |pos| (sbuf.nc()[pos as usize - 1], sbuf.nc()[pos as usize]);
        let shc = |pos| sc(pos) as usize & 0x7f | (u8::is_ascii_alphanumeric(&sbuf.nc()[pos - 1]) as usize) << 7;
        let shw = |pos| sc(pos) as usize & 0x7f | shc(pos - 1) << 7;

        // init mtf array
        if self.ctx.mtfs.is_empty() {
            self.ctx.mtfs = vec![
                MTFCoder::from_vs(&(0..super::MTF_NUM_SYMBOLS)
                    .map(|_| tbuf.read_forward(&mut tpos))
                    .map(u16::from_le)
                    .collect::<Vec<_>>());
                512];
        }

        // decode sbuf_len/match_items_len
        let sbuf = std::slice::from_raw_parts_mut(sbuf.as_ptr() as *mut u8, 0);
        bits.load_u32(tbuf, &mut tpos);
        bits.load_u32(tbuf, &mut tpos);
        let sbuf_len = bits.get(32) as usize;
        let match_items_len = bits.get(32) as usize;

        // start decoding
        let huff_decoder1 = HuffmanDecoder::new(super::MTF_NUM_SYMBOLS, tbuf, &mut tpos);
        let huff_decoder2 = HuffmanDecoder::new(super::LZ_MATCH_MAX_LEN, tbuf, &mut tpos);
        for _ in 0 .. match_items_len {
            let last_word_expected = self.ctx.words.nc()[shw(spos - 1)];
            let mtf = &mut self.ctx.mtfs.nc_mut()[(self.ctx.after_literal as usize) << 8 | shc(spos - 1)];
            let mtf_unlikely = last_word_expected.0;

            bits.load_u32(tbuf, &mut tpos);
            let symbol = huff_decoder1.decode_from_bits(&mut bits);
            if !(0 ..= super::MTF_NUM_SYMBOLS as u16).contains(&symbol) {
                Err(())?;
            }

            match mtf.decode(symbol, mtf_unlikely as u16) {
                WORD_SYMBOL => {
                    self.ctx.buckets.nc_mut()[shc(spos - 1)].update(spos, 0, 0);
                    self.ctx.after_literal = false;
                    sbuf.write_forward(&mut spos, last_word_expected);
                }
                symbol @ 0 ..= 255 => {
                    self.ctx.buckets.nc_mut()[shc(spos - 1)].update(spos, 0, 0);
                    self.ctx.after_literal = true;
                    sbuf.write_forward(&mut spos, symbol as u8);
                    self.ctx.words.nc_mut()[shw(spos - 3)] = sw(spos - 1);
                }
                encoded_roid_lenid @ _ => {
                    let (roid, lenid) = (
                        ((encoded_roid_lenid - 256) / super::LZ_LENID_SIZE as u16) as u8,
                        ((encoded_roid_lenid - 256) % super::LZ_LENID_SIZE as u16) as u8,
                    );

                    // get reduced offset
                    let (robase, robitlen) = LZ_ROID_DECODING_ARRAY.nc()[roid as usize];
                    let reduced_offset = robase + bits.get(robitlen) as u16;

                    // get match_pos/match_len
                    let match_info = self.ctx.buckets.nc()[shc(spos - 1)].get_match_pos_and_match_len(reduced_offset);
                    let encoded_match_len = if lenid == super::LZ_LENID_SIZE as u8 - 1 {
                        bits.load_u32(tbuf, &mut tpos);
                        huff_decoder2.decode_from_bits(&mut bits) as usize
                    } else {
                        lenid as usize
                    };
                    let (match_pos, match_len_expected, match_len_min) = match_info;
                    let match_len = match encoded_match_len {
                        l if l + match_len_min > match_len_expected => l + match_len_min,
                        l if l > 0 => encoded_match_len + match_len_min - 1,
                        _ => match_len_expected,
                    };
                    self.ctx.buckets.nc_mut()[shc(spos - 1)].update(spos, reduced_offset, match_len);
                    self.ctx.after_literal = false;

                    super::mem::copy_fast(sbuf, match_pos, spos, match_len);
                    spos += match_len;
                    self.ctx.words.nc_mut()[shw(spos - 3)] = sw(spos - 1);
                }
            }
        }
        return Ok((std::cmp::min(spos, sbuf_len), std::cmp::min(tpos, tbuf.len())));
    }
}
