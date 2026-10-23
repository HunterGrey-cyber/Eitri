use crate::gtk_host::instance_pool::Retire;

pub(crate) trait Pane {
    fn close(&self);
}

impl<P: Pane> Retire for P {
    fn on_retire(&self) {
        self.close();
    }
}
