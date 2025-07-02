use jack::{AudioIn, AudioOut, Client, ClientOptions, Control, Port, ProcessHandler};
use ringbuf::HeapRb;
use ringbuf::traits::{Consumer, Observer, Producer, Split};
use std::net::UdpSocket;
use std::thread;
use std::time::Duration;

// Constants
const UDP_PORT: u16 = 2138;
const UDP_PORT2: u16 = 2137;
const RECEIVING_IP: &str = "192.168.0.135";
const SENDING_IP: &str = "192.168.0.135";
const UDP_PACKET_SIZE: usize = 512;
const BUFFER_SIZE: usize = 48000 * 2;
const BYTES_PER_SAMPLE: usize = 4;
const CHANNELS: usize = 2;

// Generic struct with proper trait bounds
struct CaptureProcess<PL, PR>
where
    PL: Producer<Item = f32> + Send,
    PR: Producer<Item = f32> + Send,
{
    in_port_l: Port<AudioIn>,
    in_port_r: Port<AudioIn>,
    producer_l: PL,
    producer_r: PR,
}

impl<PL, PR> ProcessHandler for CaptureProcess<PL, PR>
where
    PL: Producer<Item = f32> + Send,
    PR: Producer<Item = f32> + Send,
{
    fn process(&mut self, _: &Client, ps: &jack::ProcessScope) -> Control {
        let in_l = self.in_port_l.as_slice(ps);
        let in_r = self.in_port_r.as_slice(ps);

        for &sample in in_l {
            let _ = self.producer_l.try_push(sample);
        }
        for &sample in in_r {
            let _ = self.producer_r.try_push(sample);
        }

        Control::Continue
    }
}

// Generic struct with proper trait bounds
struct PlaybackProcess<CL, CR>
where
    CL: Consumer<Item = f32> + Send,
    CR: Consumer<Item = f32> + Send,
{
    out_port_l: Port<AudioOut>,
    out_port_r: Port<AudioOut>,
    consumer_l: CL,
    consumer_r: CR,
}

impl<CL, CR> ProcessHandler for PlaybackProcess<CL, CR>
where
    CL: Consumer<Item = f32> + Send,
    CR: Consumer<Item = f32> + Send,
{
    fn process(&mut self, _: &Client, ps: &jack::ProcessScope) -> Control {
        let out_l = self.out_port_l.as_mut_slice(ps);
        let out_r = self.out_port_r.as_mut_slice(ps);

        for sample in out_l.iter_mut() {
            *sample = self.consumer_l.try_pop().unwrap_or(0.0);
        }
        for sample in out_r.iter_mut() {
            *sample = self.consumer_r.try_pop().unwrap_or(0.0);
        }

        Control::Continue
    }
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    // Create ring buffers with mutability for network threads
    let cap_rb_l = HeapRb::<f32>::new(BUFFER_SIZE);
    let cap_rb_r = HeapRb::<f32>::new(BUFFER_SIZE);
    let (cap_prod_l, mut cap_cons_l) = cap_rb_l.split();
    let (cap_prod_r, mut cap_cons_r) = cap_rb_r.split();

    let play_rb_l = HeapRb::<f32>::new(BUFFER_SIZE);
    let play_rb_r = HeapRb::<f32>::new(BUFFER_SIZE);
    let (mut play_prod_l, play_cons_l) = play_rb_l.split();
    let (mut play_prod_r, play_cons_r) = play_rb_r.split();

    // Create capture client
    let (client_capture, _) = Client::new("capture_client", ClientOptions::NO_START_SERVER)?;
    let in_port_l = client_capture.register_port("input_left", AudioIn::default())?;
    let in_port_r = client_capture.register_port("input_right", AudioIn::default())?;

    let capture_process = CaptureProcess {
        in_port_l,
        in_port_r,
        producer_l: cap_prod_l,
        producer_r: cap_prod_r,
    };

    let _active_capture = client_capture.activate_async((), capture_process)?;

    // Create playback client
    let (client_playback, _) = Client::new("playback_client", ClientOptions::NO_START_SERVER)?;
    let out_port_l = client_playback.register_port("output_left", AudioOut::default())?;
    let out_port_r = client_playback.register_port("output_right", AudioOut::default())?;

    let playback_process = PlaybackProcess {
        out_port_l,
        out_port_r,
        consumer_l: play_cons_l,
        consumer_r: play_cons_r,
    };

    let _active_playback = client_playback.activate_async((), playback_process)?;

    println!("JACK clients running. Starting network threads...");

    // Network sender thread (capture → network)
    let sender_socket = UdpSocket::bind("0.0.0.0:0")?;
    sender_socket.connect((SENDING_IP, UDP_PORT))?;

    thread::spawn(move || {
        let mut packet = vec![0u8; UDP_PACKET_SIZE * BYTES_PER_SAMPLE * CHANNELS];
        let mut samples_l = [0f32; UDP_PACKET_SIZE];
        let mut samples_r = [0f32; UDP_PACKET_SIZE];

        loop {
            if cap_cons_l.occupied_len() < UDP_PACKET_SIZE
                || cap_cons_r.occupied_len() < UDP_PACKET_SIZE
            {
                thread::sleep(Duration::from_micros(100));
                continue;
            }

            for i in 0..UDP_PACKET_SIZE {
                samples_l[i] = cap_cons_l.try_pop().expect("Data available");
                samples_r[i] = cap_cons_r.try_pop().expect("Data available");
            }

            let mut idx = 0;
            for i in 0..UDP_PACKET_SIZE {
                let bytes_l = samples_l[i].to_le_bytes();
                let bytes_r = samples_r[i].to_le_bytes();
                packet[idx..idx + 4].copy_from_slice(&bytes_l);
                packet[idx + 4..idx + 8].copy_from_slice(&bytes_r);
                idx += 8;
            }

            if let Err(e) = sender_socket.send(&packet) {
                eprintln!("Network send error: {}", e);
            }
        }
    });

    // Network receiver thread (network → playback)
    let receiver_socket = UdpSocket::bind((RECEIVING_IP, UDP_PORT2))?;
    receiver_socket.set_nonblocking(false)?;

    thread::spawn(move || {
        let mut packet = vec![0u8; UDP_PACKET_SIZE * BYTES_PER_SAMPLE * CHANNELS];
        let mut samples_l = [0f32; UDP_PACKET_SIZE];
        let mut samples_r = [0f32; UDP_PACKET_SIZE];

        loop {
            match receiver_socket.recv(&mut packet) {
                Ok(size) if size == packet.len() => {
                    let mut idx = 0;
                    for i in 0..UDP_PACKET_SIZE {
                        samples_l[i] = f32::from_le_bytes([
                            packet[idx],
                            packet[idx + 1],
                            packet[idx + 2],
                            packet[idx + 3],
                        ]);
                        samples_r[i] = f32::from_le_bytes([
                            packet[idx + 4],
                            packet[idx + 5],
                            packet[idx + 6],
                            packet[idx + 7],
                        ]);
                        idx += 8;
                    }

                    for i in 0..UDP_PACKET_SIZE {
                        let _ = play_prod_l.try_push(samples_l[i]);
                        let _ = play_prod_r.try_push(samples_r[i]);
                    }
                }
                Ok(size) => eprintln!("Malformed packet (size: {})", size),
                Err(e) => {
                    eprintln!("Network receive error: {}", e);
                    thread::sleep(Duration::from_millis(1));
                }
            }
        }
    });

    // Keep main thread alive
    loop {
        thread::sleep(Duration::from_secs(1));
    }
}
