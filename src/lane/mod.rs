mod op;
mod pool;
mod sweeper;
mod waiter;

pub use op::Op;
pub use pool::{NUM_LANES, start, submit as begin};
pub use waiter::Waiter;

#[cfg(test)]
pub(crate) mod tests {
    use std::sync::mpsc::channel;
    use std::time::Duration;

    use super::*;
    use crate::ascii::{self, Tokens};
    use crate::data::vector::Vector;
    use crate::index;
    pub use crate::lane::pool::tests::start_once;
    use crate::memcached::server::fake;

    #[test]
    fn begin_replies_from_the_io_callback_after_a_lane_notifies() {
        fake::install();
        start_once();
        let (tx, rx) = channel();
        let conn = fake::Conn::new(7, &tx);
        let config = index::tests::test_config(4, "cos", "f32");
        assert_eq!(index::create("aio", 0, &config), Ok(()));
        let tokens = Tokens::from_line("vop search aio vector 16 3");
        let mut request = ascii::parse(tokens.words()).unwrap();
        request.set_vector(Vector::F32(vec![0.0; 4]));
        begin(
            Op::new(request).unwrap(),
            Some(Waiter::new(conn.connection())),
        );
        assert_eq!(rx.recv_timeout(Duration::from_secs(10)).unwrap(), 7);
        assert!(conn.reply().is_empty());
        assert!(conn.run_callback());
        assert_eq!(conn.reply(), b"VECTORS 0\r\nEND\r\n");
    }
}
