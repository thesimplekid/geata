use std::{collections::HashMap, sync::Arc};

use arc_swap::ArcSwap;
use parking_lot::RwLock;

use crate::{certificates::Certificate, config::Config};

pub struct State {
    pub config: ArcSwap<Config>,
    pub certificates: RwLock<HashMap<String, Arc<Certificate>>>,
    pub challenges: RwLock<HashMap<(String, String), String>>,
}

impl State {
    pub fn new(config: Config) -> Self {
        Self {
            config: ArcSwap::from_pointee(config),
            certificates: RwLock::new(HashMap::new()),
            challenges: RwLock::new(HashMap::new()),
        }
    }
}
