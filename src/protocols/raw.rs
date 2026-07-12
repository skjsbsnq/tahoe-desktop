pub mod mutter_x11_interop {
    pub mod v1 {
        pub use self::generated::server;

        mod generated {
            pub mod server {
                #![allow(dead_code, non_camel_case_types, unused_unsafe, unused_variables)]
                #![allow(non_upper_case_globals, non_snake_case, unused_imports)]
                #![allow(missing_docs, clippy::all)]

                use smithay::reexports::wayland_server;
                use wayland_server::protocol::*;

                pub mod __interfaces {
                    use smithay::reexports::wayland_server;
                    use wayland_server::protocol::__interfaces::*;
                    wayland_scanner::generate_interfaces!("resources/mutter-x11-interop.xml");
                }
                use self::__interfaces::*;

                wayland_scanner::generate_server_code!("resources/mutter-x11-interop.xml");
            }
        }
    }
}

pub mod tahoe_glass {
    pub mod v1 {
        #[cfg(test)]
        pub use self::generated::client;
        pub use self::generated::server;

        mod generated {
            #[cfg(test)]
            pub mod client {
                #![allow(dead_code, non_camel_case_types, unused_unsafe, unused_variables)]
                #![allow(non_upper_case_globals, non_snake_case, unused_imports)]
                #![allow(missing_docs, clippy::all)]

                use wayland_client;
                use wayland_client::protocol::*;

                pub mod __interfaces {
                    use wayland_client;
                    use wayland_client::protocol::__interfaces::*;
                    wayland_scanner::generate_interfaces!("resources/tahoe-glass-v1.xml");
                }
                use self::__interfaces::*;

                wayland_scanner::generate_client_code!("resources/tahoe-glass-v1.xml");
            }

            pub mod server {
                #![allow(dead_code, non_camel_case_types, unused_unsafe, unused_variables)]
                #![allow(non_upper_case_globals, non_snake_case, unused_imports)]
                #![allow(missing_docs, clippy::all)]

                use smithay::reexports::wayland_server;
                use wayland_server::protocol::*;

                pub mod __interfaces {
                    use smithay::reexports::wayland_server;
                    use wayland_server::protocol::__interfaces::*;
                    wayland_scanner::generate_interfaces!("resources/tahoe-glass-v1.xml");
                }
                use self::__interfaces::*;

                wayland_scanner::generate_server_code!("resources/tahoe-glass-v1.xml");
            }
        }
    }
}
