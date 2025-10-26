use crate::array::{Array, PrimitiveArray};
use crate::types::NativeType;

pub fn ewm_mean<I, T>(
    xs: I,
    alpha: T,
    adjust: bool,
    min_periods: usize,
    ignore_nulls: bool,
) -> PrimitiveArray<T>
where
    I: IntoIterator<Item = Option<T>>,
    T: num_traits::Float + NativeType + std::ops::MulAssign,
{
    let mut state: EwmMeanState<T> = EwmMeanState::new(alpha, adjust, min_periods, ignore_nulls);
    state.update_iter(xs).collect()
}

pub struct EwmMeanState<T> {
    weighted_mean: T,
    weight: T,
    alpha: T,
    non_null_count: usize,
    adjust: bool,
    min_periods: usize,
    ignore_nulls: bool,
}

impl<T> EwmMeanState<T>
where
    T: num_traits::Float,
{
    pub fn new(alpha: T, adjust: bool, min_periods: usize, ignore_nulls: bool) -> Self {
        Self {
            weighted_mean: T::zero(),
            weight: T::zero(),
            alpha,
            non_null_count: 0,
            adjust,
            min_periods: min_periods.max(1),
            ignore_nulls,
        }
    }
}

impl<T> EwmMeanState<T>
where
    T: NativeType + num_traits::Float + std::ops::MulAssign,
{
    pub fn update(&mut self, values: &PrimitiveArray<T>) -> PrimitiveArray<T> {
        self.update_iter(values.iter().map(|x| x.copied()))
            .collect()
    }

    pub fn update_iter<I>(&mut self, values: I) -> impl Iterator<Item = Option<T>>
    where
        I: IntoIterator<Item = Option<T>>,
    {
        let new_value_weight = if self.adjust { T::one() } else { self.alpha };

        values.into_iter().map(move |opt_v| {
            if self.non_null_count == 0
                && let Some(v) = opt_v
            {
                // Initialize
                self.non_null_count = 1;
                self.weighted_mean = v;
                self.weight = T::one();
            } else {
                if opt_v.is_some() || !self.ignore_nulls {
                    self.weight *= T::one() - self.alpha;
                }

                if let Some(new_v) = opt_v {
                    let new_weight = self.weight + new_value_weight;

                    self.weighted_mean = self.weighted_mean
                        + (new_v - self.weighted_mean) * (new_value_weight / new_weight);

                    self.weight = if self.adjust {
                        self.weight + T::one()
                    } else {
                        T::one()
                    };

                    self.non_null_count += 1;
                }
            }

            (opt_v.is_some() && self.non_null_count >= self.min_periods)
                .then_some(self.weighted_mean)
        })
    }
}

pub enum DynEwmMeanState {
    F32(EwmMeanState<f32>),
    F64(EwmMeanState<f64>),
}

impl DynEwmMeanState {
    pub fn update(&mut self, values: &dyn Array) -> Box<dyn Array> {
        match self {
            Self::F32(state) => state
                .update(values.as_any().downcast_ref().unwrap())
                .boxed(),
            Self::F64(state) => state
                .update(values.as_any().downcast_ref().unwrap())
                .boxed(),
        }
    }
}

impl From<EwmMeanState<f32>> for DynEwmMeanState {
    fn from(value: EwmMeanState<f32>) -> Self {
        Self::F32(value)
    }
}

impl From<EwmMeanState<f64>> for DynEwmMeanState {
    fn from(value: EwmMeanState<f64>) -> Self {
        Self::F64(value)
    }
}

#[cfg(test)]
mod test {
    use super::super::assert_allclose;
    use super::*;
    const ALPHA: f64 = 0.5;
    const EPS: f64 = 1e-15;

    #[test]
    fn test_ewm_mean_without_null() {
        let xs: Vec<Option<f64>> = vec![Some(1.0), Some(2.0), Some(3.0)];
        for adjust in [false, true] {
            for ignore_nulls in [false, true] {
                for min_periods in [0, 1] {
                    let result = ewm_mean(xs.clone(), ALPHA, adjust, min_periods, ignore_nulls);
                    let expected = match adjust {
                        false => PrimitiveArray::from([Some(1.0f64), Some(1.5f64), Some(2.25f64)]),
                        true => PrimitiveArray::from([
                            Some(1.0),
                            Some(1.666_666_666_666_666_7),
                            Some(2.428_571_428_571_428_4),
                        ]),
                    };
                    assert_allclose!(result, expected, 1e-15);
                }
                let result = ewm_mean(xs.clone(), ALPHA, adjust, 2, ignore_nulls);
                let expected = match adjust {
                    false => PrimitiveArray::from([None, Some(1.5f64), Some(2.25f64)]),
                    true => PrimitiveArray::from([
                        None,
                        Some(1.666_666_666_666_666_7),
                        Some(2.428_571_428_571_428_4),
                    ]),
                };
                assert_allclose!(result, expected, EPS);
            }
        }
    }

    #[test]
    fn test_ewm_mean_with_null() {
        let xs1 = vec![
            None,
            None,
            Some(5.0f64),
            Some(7.0f64),
            None,
            Some(2.0f64),
            Some(1.0f64),
            Some(4.0f64),
        ];
        assert_allclose!(
            ewm_mean(xs1.clone(), 0.5, true, 0, true),
            PrimitiveArray::from([
                None,
                None,
                Some(5.0),
                Some(6.333_333_333_333_333),
                None,
                Some(3.857_142_857_142_857),
                Some(2.333_333_333_333_333_5),
                Some(3.193_548_387_096_774),
            ]),
            EPS
        );
        assert_allclose!(
            ewm_mean(xs1.clone(), 0.5, true, 0, false),
            PrimitiveArray::from([
                None,
                None,
                Some(5.0),
                Some(6.333_333_333_333_333),
                None,
                Some(3.181_818_181_818_181_7),
                Some(1.888_888_888_888_888_8),
                Some(3.033_898_305_084_745_7),
            ]),
            EPS
        );
        assert_allclose!(
            ewm_mean(xs1.clone(), 0.5, false, 0, true),
            PrimitiveArray::from([
                None,
                None,
                Some(5.0),
                Some(6.0),
                None,
                Some(4.0),
                Some(2.5),
                Some(3.25),
            ]),
            EPS
        );
        assert_allclose!(
            ewm_mean(xs1, 0.5, false, 0, false),
            PrimitiveArray::from([
                None,
                None,
                Some(5.0),
                Some(6.0),
                None,
                Some(3.333_333_333_333_333_5),
                Some(2.166_666_666_666_667),
                Some(3.083_333_333_333_333_5),
            ]),
            EPS
        );
    }
}
