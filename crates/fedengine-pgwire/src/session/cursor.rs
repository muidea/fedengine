use fedengine_contract::*;
use fedengine_core::EngineError;
use fedengine_pg_compat::pg_error;
use pgwire::{api::results::Tag, error::PgWireResult};
use std::collections::VecDeque;
pub(super) struct Cursor {
    pub(super) columns: Vec<ColumnMeta>,
    pub(super) stream: Option<ExecutionStream>,
    pub(super) pending: VecDeque<Vec<Cell>>,
    pub(super) completion: Option<Completion>,
    pub(super) total: u64,
    pub(super) kind: StatementKind,
}
impl Cursor {
    pub(super) async fn next(&mut self) -> PgWireResult<Option<Vec<Cell>>> {
        loop {
            if let Some(row) = self.pending.pop_front() {
                self.total += 1;
                return Ok(Some(row));
            }
            let Some(s) = &mut self.stream else {
                return Ok(None);
            };
            let chunk = tokio::select! {biased;_=s.cancel.cancelled()=>return Err(pg_error(if self.kind.writes(){EngineError::new("57014","write canceled").unknown()}else{EngineError::new("57014","query canceled")})),chunk=s.receiver.recv()=>chunk};
            match chunk {
                Some(ResultChunk::Rows(rows)) => self.pending.extend(rows),
                Some(ResultChunk::Complete(c)) => {
                    self.completion = Some(c);
                    self.stream.take();
                    return Ok(None);
                }
                Some(ResultChunk::Error(e)) => return Err(pg_error(e)),
                None => {
                    return Err(pg_error(EngineError::new(
                        "XX000",
                        "worker closed without a terminal completion",
                    )));
                }
            }
        }
    }
    pub(super) fn tag(&self) -> Tag {
        let c = self.completion.as_ref().unwrap();
        let tag = Tag::new(&c.command);
        if c.command == "INSERT" {
            tag.with_oid(0)
                .with_rows(c.affected_rows.unwrap_or(0) as usize)
        } else if let Some(n) = c.affected_rows {
            tag.with_rows(n as usize)
        } else {
            tag
        }
    }
}
