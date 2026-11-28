//! Смешение нескольких захватов звука в один поток кадров по 10 мс.

use std::collections::VecDeque;
use std::sync::Mutex;

use crate::app_audio::AudioSink;

/// Больше стольких кадров в очереди — источник опережает часы смесителя: старые выкидываются.
pub const MIX_QUEUE_MAX: usize = 10;
/// Сколько свежих кадров оставить после сброса.
pub const MIX_QUEUE_KEEP: usize = 3;

/// Очередь кадров одного захвата (его приёмник).
#[derive(Default)]
pub struct MixQueue {
    q: Mutex<VecDeque<Vec<i16>>>,
}

impl MixQueue {
    pub fn pop(&self) -> Option<Vec<i16>> {
        self.q.lock().unwrap_or_else(|e| e.into_inner()).pop_front()
    }

    pub fn len(&self) -> usize {
        self.q.lock().unwrap_or_else(|e| e.into_inner()).len()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

impl AudioSink for MixQueue {
    fn frame(&self, data: &[i16]) {
        let mut q = self.q.lock().unwrap_or_else(|e| e.into_inner());
        q.push_back(data.to_vec());
        if q.len() > MIX_QUEUE_MAX {
            let drop = q.len() - MIX_QUEUE_KEEP;
            q.drain(..drop);
        }
    }
}

/// Сумма кадров с насыщением; без кадров — тишина.
pub fn mix_into(out: &mut [i16], frames: &[Vec<i16>]) {
    for (i, o) in out.iter_mut().enumerate() {
        let s: i32 = frames.iter().map(|f| f.get(i).copied().unwrap_or(0) as i32).sum();
        *o = s.clamp(i16::MIN as i32, i16::MAX as i32) as i16;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mix_saturates() {
        let mut out = [0i16; 2];
        mix_into(&mut out, &[vec![30000, -30000], vec![30000, -30000]]);
        assert_eq!(out, [32767, -32768]);
        mix_into(&mut out, &[vec![100, -5], vec![20, 5]]);
        assert_eq!(out, [120, 0]);
    }

    #[test]
    fn mix_empty_is_silence() {
        let mut out = [7i16; 4];
        mix_into(&mut out, &[]);
        assert_eq!(out, [0; 4]);
    }

    #[test]
    fn queue_caps_and_keeps_newest() {
        let q = MixQueue::default();
        for v in 0..12i16 {
            q.frame(&[v, v]);
        }
        let got: Vec<i16> = std::iter::from_fn(|| q.pop()).map(|f| f[0]).collect();
        // 11-й кадр переполнил очередь → остались 8, 9, 10; затем пришёл 11
        assert_eq!(got, vec![8, 9, 10, 11]);
    }
}
