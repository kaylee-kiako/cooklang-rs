use miette::Diagnostic;
use thiserror::Error;

use crate::{
    quantity::{QuantityValue, ScalableValue, TextValueError, Value},
    Recipe,
};

#[derive(Debug, Clone, Copy)]
pub struct ScaleTarget {
    base: u32,
    target: u32,
    index: Option<usize>,
}

impl ScaleTarget {
    pub fn new(base: u32, target: u32, declared_servings: &[u32]) -> Self {
        ScaleTarget {
            base,
            target,
            index: declared_servings.iter().position(|&s| s == target),
        }
    }

    pub fn factor(&self) -> f64 {
        self.target as f64 / self.base as f64
    }

    pub fn index(&self) -> Option<usize> {
        self.index
    }

    pub fn target_servings(&self) -> u32 {
        self.target
    }
}

#[derive(Debug)]
pub enum Scaled {
    SkippedSacaling,
    Scaled(ScaledData),
}

#[derive(Debug)]
pub struct ScaledData {
    pub target: ScaleTarget,
    pub ingredients: Vec<ScaleOutcome>,
    pub cookware: Vec<ScaleOutcome>,
    pub timers: Vec<ScaleOutcome>,
}

#[derive(Debug, Clone)]
pub enum ScaleOutcome {
    Scaled,
    Fixed,
    NoQuantity,
    Error(ScaleError),
}

pub type ScaledRecipe<'a> = Recipe<'a, Scaled>;

#[derive(Debug, Error, Diagnostic, Clone)]
pub enum ScaleError {
    #[error(transparent)]
    #[diagnostic(transparent)]
    TextValueError(#[from] TextValueError),

    #[error("Value not scalable: {reason}")]
    NotScalable {
        value: ScalableValue<'static>,
        reason: &'static str,
    },

    #[error("Value scaling not defined for target servings")]
    NotDefined {
        target: ScaleTarget,
        value: ScalableValue<'static>,
    },
}

impl<'a> Recipe<'a> {
    pub fn scale(mut self, target: ScaleTarget) -> ScaledRecipe<'a> {
        let ingredients = scale_many(target, &mut self.ingredients, |igr| {
            igr.quantity.as_mut().map(|q| &mut q.value)
        });
        let cookware = scale_many(target, &mut self.cookware, |ck| ck.quantity.as_mut());
        let timers = scale_many(target, &mut self.timers, |tm| Some(&mut tm.quantity.value));

        let data = ScaledData {
            target,
            ingredients,
            cookware,
            timers,
        };

        ScaledRecipe {
            name: self.name,
            metadata: self.metadata,
            sections: self.sections,
            ingredients: self.ingredients,
            cookware: self.cookware,
            timers: self.timers,
            data: Scaled::Scaled(data),
        }
    }

    pub fn skip_scaling(self) -> ScaledRecipe<'a> {
        ScaledRecipe {
            name: self.name,
            metadata: self.metadata,
            sections: self.sections,
            ingredients: self.ingredients,
            cookware: self.cookware,
            timers: self.timers,
            data: Scaled::SkippedSacaling,
        }
    }
}

impl ScaledRecipe<'_> {
    pub fn scaled_data(&self) -> Option<&ScaledData> {
        if let Scaled::Scaled(data) = &self.data {
            Some(data)
        } else {
            None
        }
    }
}

fn scale_many<'a, T: 'a>(
    target: ScaleTarget,
    components: &mut [T],
    extract: impl Fn(&mut T) -> Option<&mut QuantityValue<'a>>,
) -> Vec<ScaleOutcome> {
    let mut outcomes = Vec::with_capacity(components.len());
    for c in components {
        if let Some(value) = extract(c) {
            match value.clone().scale(target) {
                // ? Unnecesary clone maybe
                Ok((v, o)) => {
                    *value = v;
                    outcomes.push(o);
                }
                Err(e) => outcomes.push(ScaleOutcome::Error(e)),
            }
        } else {
            outcomes.push(ScaleOutcome::NoQuantity);
        }
    }
    outcomes
}

impl<'a> QuantityValue<'a> {
    fn scale(self, target: ScaleTarget) -> Result<(QuantityValue<'a>, ScaleOutcome), ScaleError> {
        match self {
            v @ QuantityValue::Fixed(_) => Ok((v, ScaleOutcome::Fixed)),
            QuantityValue::Scalable(v) => {
                v.scale(target).map(|(v, o)| (QuantityValue::Fixed(v), o))
            }
        }
    }
}

impl<'a> ScalableValue<'a> {
    fn scale(self, target: ScaleTarget) -> Result<(Value<'a>, ScaleOutcome), ScaleError> {
        match self {
            ScalableValue::Linear(v) => Ok((v.scale(target.factor())?, ScaleOutcome::Scaled)),
            ScalableValue::ByServings(ref v) => {
                if let Some(index) = target.index {
                    let Some(value) = v.get(index) else {
                        return Err(ScaleError::NotDefined { target, value: self.into_owned() });
                    };
                    Ok((value.clone(), ScaleOutcome::Scaled))
                } else {
                    return Err(ScaleError::NotScalable {
                        value: self.into_owned(),
                        reason: "tried to scale a value linearly when it has the scaling defined",
                    });
                }
            }
        }
    }
}

impl Value<'_> {
    fn scale(&self, factor: f64) -> Result<Value<'static>, ScaleError> {
        match self.clone() {
            Value::Number(n) => Ok(Value::Number(n * factor)),
            Value::Range(r) => Ok(Value::Range(r.start() * factor..=r.end() * factor)),
            v @ Value::Text(_) => return Err(TextValueError(v.into_owned()).into()),
        }
    }
}