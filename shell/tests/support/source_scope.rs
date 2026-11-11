//! Removes the loop's remaining sources before its widgets are destroyed.

pub struct SourceScope {
    context: glib::MainContext,
    ids: Vec<glib::SourceId>,
}

impl SourceScope {
    pub fn new(context: glib::MainContext, ids: Vec<glib::SourceId>) -> Self {
        Self { context, ids }
    }
}

impl Drop for SourceScope {
    fn drop(&mut self) {
        for id in &self.ids {
            if let Some(source) = self.context.find_source_by_id(id) {
                source.destroy();
            }
        }
    }
}
