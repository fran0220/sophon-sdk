use super::*;

pub(in crate::private) fn retain_durable_event(
    sequences: &Rc<RefCell<HashMap<String, u64>>>,
    retained: &Rc<RefCell<HashMap<String, VecDeque<Event>>>>,
    generations: &Rc<RefCell<HashMap<String, u64>>>,
    store: &Arc<dyn crate::SessionEventJournalStore>,
    capacity: usize,
    session_id: SessionId,
    turn_id: Option<String>,
    replay: bool,
    update: EventUpdate,
) -> Result<Event, Error> {
    if !generations.borrow().contains_key(session_id.as_str()) {
        let snapshot = match store.snapshot(session_id.as_str()).map_err(op)? {
            Some(snapshot) => snapshot,
            None => {
                match store.initialize(session_id.as_str(), 0).map_err(op)? {
                    crate::EventJournalCommit::Committed
                    | crate::EventJournalCommit::Conflict
                    | crate::EventJournalCommit::CommitUnknown => {}
                }
                store
                    .snapshot(session_id.as_str())
                    .map_err(op)?
                    .ok_or_else(|| {
                        Error::Operation("initialized event journal is unavailable".into())
                    })?
            }
        };
        if snapshot.status != crate::EventJournalStatus::Ready && !replay {
            return Err(Error::Operation(
                "event journal rebuild did not finish".into(),
            ));
        }
        install_event_journal_snapshot(sequences, retained, generations, &session_id, snapshot)?;
    }
    let expected_head = sequences
        .borrow()
        .get(session_id.as_str())
        .copied()
        .ok_or_else(|| Error::Operation("event journal head is unavailable".into()))?;
    let generation = generations
        .borrow()
        .get(session_id.as_str())
        .copied()
        .ok_or_else(|| Error::Operation("event journal generation is unavailable".into()))?;
    let sequence = expected_head
        .checked_add(1)
        .ok_or_else(|| Error::Operation("event journal sequence overflow".into()))?;
    let event = Event {
        session_id: session_id.clone(),
        sequence,
        turn_id,
        timestamp_ms: now_ms(),
        replay,
        update,
    };
    let bytes = serde_json::to_vec(&event).map_err(op)?;
    let append = crate::EventJournalAppend {
        session_id: session_id.as_str().to_owned(),
        generation,
        expected_head,
        event: crate::StoredSessionEvent {
            sequence,
            bytes: bytes.clone(),
        },
        capacity,
    };
    let committed = match store.append(&append).map_err(op)? {
        crate::EventJournalCommit::Committed => true,
        crate::EventJournalCommit::Conflict => false,
        crate::EventJournalCommit::CommitUnknown => store
            .snapshot(session_id.as_str())
            .map_err(op)?
            .is_some_and(|snapshot| {
                snapshot.generation == generation
                    && snapshot.inclusive_end_sequence == sequence
                    && snapshot
                        .retained
                        .last()
                        .is_some_and(|stored| stored.sequence == sequence && stored.bytes == bytes)
            }),
    };
    if !committed {
        return Err(Error::Operation(
            "event journal append conflicted or could not be reconciled".into(),
        ));
    }
    sequences
        .borrow_mut()
        .insert(session_id.as_str().to_owned(), sequence);
    let mut journals = retained.borrow_mut();
    let journal = journals.entry(session_id.as_str().to_owned()).or_default();
    journal.push_back(event.clone());
    while journal.len() > capacity {
        journal.pop_front();
    }
    Ok(event)
}

fn install_event_journal_snapshot(
    sequences: &Rc<RefCell<HashMap<String, u64>>>,
    retained: &Rc<RefCell<HashMap<String, VecDeque<Event>>>>,
    generations: &Rc<RefCell<HashMap<String, u64>>>,
    id: &SessionId,
    snapshot: crate::EventJournalSnapshot,
) -> Result<(), Error> {
    let mut events = VecDeque::with_capacity(snapshot.retained.len());
    for stored in snapshot.retained {
        let event: Event = serde_json::from_slice(&stored.bytes).map_err(op)?;
        if event.session_id != *id || event.sequence != stored.sequence {
            return Err(Error::Operation(
                "event journal payload identity differs from its index".into(),
            ));
        }
        events.push_back(event);
    }
    sequences
        .borrow_mut()
        .insert(id.as_str().to_owned(), snapshot.inclusive_end_sequence);
    retained.borrow_mut().insert(id.as_str().to_owned(), events);
    generations
        .borrow_mut()
        .insert(id.as_str().to_owned(), snapshot.generation);
    Ok(())
}

impl Core {
    pub(super) fn initialize_event_journal(&self, id: &SessionId) -> Result<(), Error> {
        let commit = self
            .event_journal_store
            .initialize(id.as_str(), 0)
            .map_err(op)?;
        if commit == crate::EventJournalCommit::Conflict {
            return Err(Error::Operation(
                "new Session identity already has an event journal".into(),
            ));
        }
        let snapshot = self
            .event_journal_store
            .snapshot(id.as_str())
            .map_err(op)?
            .filter(|snapshot| {
                snapshot.generation == 1
                    && snapshot.status == crate::EventJournalStatus::Ready
                    && snapshot.inclusive_end_sequence == 0
                    && snapshot.retained.is_empty()
            })
            .ok_or_else(|| {
                Error::Operation(
                    "new Session event journal initialization could not be reconciled".into(),
                )
            })?;
        install_event_journal_snapshot(
            &self.sequences,
            &self.retained,
            &self.journal_generations,
            id,
            snapshot,
        )
    }

    pub(super) fn restore_or_adopt_event_journal(
        &self,
        id: &SessionId,
        after_sequence: u64,
    ) -> Result<(), Error> {
        let snapshot = match self.event_journal_store.snapshot(id.as_str()).map_err(op)? {
            Some(snapshot) => snapshot,
            None => {
                match self
                    .event_journal_store
                    .initialize(id.as_str(), after_sequence)
                    .map_err(op)?
                {
                    crate::EventJournalCommit::Committed
                    | crate::EventJournalCommit::Conflict
                    | crate::EventJournalCommit::CommitUnknown => {}
                }
                self.event_journal_store
                    .snapshot(id.as_str())
                    .map_err(op)?
                    .ok_or_else(|| {
                        Error::Operation("adopted Session event journal is unavailable".into())
                    })?
            }
        };
        if snapshot.status != crate::EventJournalStatus::Ready {
            return Err(Error::Operation(
                "durable Session event journal rebuild did not finish; load the Session to rebuild it"
                    .into(),
            ));
        }
        if after_sequence > snapshot.inclusive_end_sequence {
            return Err(Error::Operation(format!(
                "Host event cursor {after_sequence} is beyond durable Session journal head {}",
                snapshot.inclusive_end_sequence
            )));
        }
        install_event_journal_snapshot(
            &self.sequences,
            &self.retained,
            &self.journal_generations,
            id,
            snapshot,
        )
    }

    pub(super) fn restore_or_rebuild_event_journal(
        &self,
        id: &SessionId,
    ) -> Result<Option<u64>, Error> {
        match self.event_journal_store.snapshot(id.as_str()).map_err(op)? {
            Some(snapshot) if snapshot.status == crate::EventJournalStatus::Ready => {
                install_event_journal_snapshot(
                    &self.sequences,
                    &self.retained,
                    &self.journal_generations,
                    id,
                    snapshot,
                )?;
                Ok(None)
            }
            Some(_) | None => self.begin_event_journal_rebuild(id).map(Some),
        }
    }

    pub(super) fn begin_event_journal_rebuild(&self, id: &SessionId) -> Result<u64, Error> {
        let snapshot = self
            .event_journal_store
            .begin_rebuild(id.as_str())
            .map_err(op)?;
        let generation = snapshot.generation;
        install_event_journal_snapshot(
            &self.sequences,
            &self.retained,
            &self.journal_generations,
            id,
            snapshot,
        )?;
        Ok(generation)
    }

    pub(super) fn finish_event_journal_rebuild(
        &self,
        id: &SessionId,
        generation: u64,
    ) -> Result<(), Error> {
        let head = self
            .sequences
            .borrow()
            .get(id.as_str())
            .copied()
            .ok_or_else(|| Error::Operation("rebuilt event journal head is unavailable".into()))?;
        match self
            .event_journal_store
            .finish_rebuild(id.as_str(), generation, head)
            .map_err(op)?
        {
            crate::EventJournalCommit::Committed => Ok(()),
            crate::EventJournalCommit::Conflict => Err(Error::Operation(
                "event journal rebuild completion conflicted".into(),
            )),
            crate::EventJournalCommit::CommitUnknown => {
                let ready = self
                    .event_journal_store
                    .snapshot(id.as_str())
                    .map_err(op)?
                    .is_some_and(|snapshot| {
                        snapshot.generation == generation
                            && snapshot.status == crate::EventJournalStatus::Ready
                            && snapshot.inclusive_end_sequence == head
                    });
                if ready {
                    Ok(())
                } else {
                    Err(Error::Operation(
                        "event journal rebuild completion could not be reconciled".into(),
                    ))
                }
            }
        }
    }

    pub(super) fn delete_event_journal(&self, id: &SessionId) -> Result<(), Error> {
        match self.event_journal_store.delete(id.as_str()).map_err(op)? {
            crate::EventJournalCommit::Committed => {
                self.sequences.borrow_mut().remove(id.as_str());
                self.retained.borrow_mut().remove(id.as_str());
                self.journal_generations.borrow_mut().remove(id.as_str());
                Ok(())
            }
            crate::EventJournalCommit::Conflict => {
                Err(Error::Operation("event journal deletion conflicted".into()))
            }
            crate::EventJournalCommit::CommitUnknown => {
                if self
                    .event_journal_store
                    .snapshot(id.as_str())
                    .map_err(op)?
                    .is_some()
                {
                    return Err(Error::Operation(
                        "event journal deletion could not be reconciled".into(),
                    ));
                }
                self.sequences.borrow_mut().remove(id.as_str());
                self.retained.borrow_mut().remove(id.as_str());
                self.journal_generations.borrow_mut().remove(id.as_str());
                Ok(())
            }
        }
    }

    pub(super) fn require_resident(&self, id: &SessionId) -> Result<(), Error> {
        if self.resident.borrow().contains(&id.0) {
            Ok(())
        } else {
            Err(Error::Operation("session is not resident".into()))
        }
    }
    pub(super) fn observe_journal(
        &self,
        id: &SessionId,
        after_sequence: u64,
    ) -> Result<JournalObservation, Error> {
        let inclusive_end_sequence = self
            .sequences
            .borrow()
            .get(&id.0)
            .copied()
            .ok_or_else(|| Error::Operation("unknown session event journal".into()))?;
        let retained = self.retained.borrow();
        observe_journal_snapshot(retained.get(&id.0), inclusive_end_sequence, after_sequence)
    }
    pub(super) fn probe_session_replay(
        &self,
        id: &SessionId,
        after_sequence: u64,
    ) -> Result<SessionReplayProbe, Error> {
        self.require_resident(id)?;
        let binding = self
            .session_bindings
            .borrow()
            .get(&id.0)
            .cloned()
            .ok_or_else(|| Error::Operation("session binding is unavailable".into()))?;
        let journal = self.observe_journal(id, after_sequence)?;
        let ledger = self.load_ledger(id)?;
        Ok(SessionReplayProbe {
            binding: crate::SessionBinding {
                session_id: id.clone(),
                cwd: binding.cwd,
                model: binding.model,
                reasoning: binding.reasoning,
                harness_digest: binding.harness_digest,
            },
            requested_after_sequence: after_sequence,
            oldest_retained_sequence: journal.oldest_retained_sequence,
            inclusive_end_sequence: journal.inclusive_end_sequence,
            retained_count: journal.retained_count,
            truncated: journal.truncated,
            events: journal.events,
            ledger,
        })
    }
    pub(super) fn events_after(&self, id: &SessionId, sequence: u64) -> Result<Vec<Event>, Error> {
        let observation = self.observe_journal(id, sequence)?;
        if observation.truncated {
            return Err(Error::EventGap {
                requested: sequence,
                oldest_available: observation.oldest_retained_sequence,
                newest: observation.inclusive_end_sequence,
            });
        }
        Ok(observation.events)
    }
}
