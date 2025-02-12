use geph4_protocol::binder::protocol::BridgeDescriptor;
use itertools::Itertools;
use native_tls::{Protocol, TlsConnector};
use rand::{Rng};
use smol_timeout::TimeoutExt;
use sosistab2::{MuxPublic, MuxSecret, ObfsTlsPipe, ObfsUdpPipe, ObfsUdpPublic, Pipe};

use crate::connect::tunnel::{activity::wait_activity, TunnelStatus};

use super::{EndpointSource, TunnelCtx};
use anyhow::Context;
use std::net::SocketAddr;

use std::{convert::TryFrom, sync::Arc, time::Duration};

pub fn parse_independent_endpoint(
    endpoint: &str,
) -> anyhow::Result<(SocketAddr, x25519_dalek::PublicKey)> {
    // parse endpoint addr
    let pk_and_url = endpoint.split('@').collect::<Vec<_>>();
    let server_pk = x25519_dalek::PublicKey::from(
        <[u8; 32]>::try_from(
            hex::decode(pk_and_url.first().context("URL not in form PK@host:port")?)
                .context("PK is not hex")?,
        )
        .unwrap(),
    );
    let server_addr: SocketAddr = pk_and_url
        .get(1)
        .context("URL not in form PK@host:port")?
        .parse()
        .context("cannot parse host:port")?;
    Ok((server_addr, server_pk))
}

pub(crate) async fn get_session(ctx: TunnelCtx) -> anyhow::Result<Arc<sosistab2::Multiplex>> {
    match &ctx.endpoint {
        EndpointSource::Independent { endpoint: _ } => {
            todo!()
        }
        EndpointSource::Binder(binder_tunnel_params) => {
            let selected_exit = binder_tunnel_params
                .ccache
                .get_closest_exit(&binder_tunnel_params.exit_server.clone().unwrap_or_default())
                .await
                .context("cannot get closest exit")?;
            log::info!("using exit {}", selected_exit.hostname);
            let bridges = binder_tunnel_params
                .ccache
                .get_bridges_v2(&selected_exit.hostname, false)
                .await
                .context("cannot get bridges")?;
            if bridges.is_empty() {
                anyhow::bail!("no sosistab2 routes to {}", selected_exit.hostname)
            }
            log::debug!("{} routes", bridges.len());
            // The bridge descriptor is laid out in a rather weird format: the "sosistab_key" field is a bincode-encode tuple of the first-level cookie, and the end-to-end MuxPublic key.
            // we assume we have at least one obfsudp key
            let e2e_key: MuxPublic = {
                let mut seen = None;
                for bridge in bridges.iter() {
                    if bridge.protocol == "sosistab2-obfsudp" {
                        if let Ok(val) =
                            bincode::deserialize::<(ObfsUdpPublic, MuxPublic)>(&bridge.sosistab_key)
                        {
                            seen = Some(val.1)
                        }
                    }
                }
                seen.context("cannot deduce the sosistab2 MuxPublic of this exit")?
            };
            let multiplex = Arc::new(sosistab2::Multiplex::new(
                MuxSecret::generate(),
                Some(e2e_key),
            ));
            // add *all* the bridges!
            let sess_id = format!("sess-{}", rand::thread_rng().gen::<u128>());
            for bridge in bridges.into_iter() {
                let sess_id = sess_id.clone();
                let multiplex = multiplex.clone();
                let ctx = ctx.clone();
                smolscale::spawn(async move {
                    match connect_once(ctx, bridge.clone(), &sess_id).await {
                        Ok(pipe) => {
                            log::debug!(
                                "add initial pipe {} / {}",
                                pipe.protocol(),
                                pipe.peer_addr()
                            );
                            multiplex.add_pipe(pipe);
                        }
                        Err(err) => {
                            log::warn!(
                                "pipe creation failed for {} ({}): {:?}",
                                bridge.endpoint,
                                bridge.protocol,
                                err
                            )
                        }
                    }
                })
                .detach();
            }

            // weak here to prevent a reference cycle!
            let weak_multiplex = Arc::downgrade(&multiplex);
            let ccache = binder_tunnel_params.ccache.clone();
            let _binder_tunnel_params = binder_tunnel_params.clone();
            multiplex.add_drop_friend(smolscale::spawn(async move {
                let _dead_count = 0;
                loop {
                    let interval = Duration::from_secs_f64(rand::thread_rng().gen_range(1.0, 3.0));
                    wait_activity(Duration::from_secs(300)).await;
                    smol::Timer::after(interval).await;
                    let dead_pipes = if let Some(multiplex) = weak_multiplex.upgrade() {
                        multiplex.clear_dead_pipes()
                    } else {
                        return;
                    };
                    if !dead_pipes.is_empty() {
                        log::debug!(
                            "dead pipes: {:?}",
                            dead_pipes
                                .iter()
                                .map(|dp| (dp.protocol(), dp.peer_addr()))
                                .collect_vec()
                        );
                        let pipe_tasks = dead_pipes
                            .into_iter()
                            .map(|pipe| {
                                let ccache = ccache.clone();
                                let ctx = ctx.clone();
                                let selected_exit = selected_exit.clone();
                                let sess_id = sess_id.clone();
                                smolscale::spawn(async move {
                                    for iteration in 0u64.. {
                                        let ctx = ctx.clone();
                                        let fallible = async {
                                            let bridges = ccache
                                                .get_bridges_v2(&selected_exit.hostname, false)
                                                .await?;
                                            if bridges.is_empty() {
                                                anyhow::bail!("empty bridge list")
                                            }
                                            let selected_bridge = bridges
                                                .iter()
                                                .find(|s| {
                                                    s.exit_hostname == pipe.peer_addr()
                                                        && iteration == 0
                                                })
                                                .unwrap_or_else(|| {
                                                    &bridges[rand::thread_rng()
                                                        .gen_range(0, bridges.len())]
                                                });

                                            let connected = connect_once(
                                                ctx,
                                                selected_bridge.clone(),
                                                &sess_id,
                                            )
                                            .await?;
                                            anyhow::Ok(connected)
                                        };
                                        match fallible.await {
                                            Ok(val) => return val,
                                            Err(err) => {
                                                log::warn!("error reconnecting a pipe: {:?}", err)
                                            }
                                        }
                                    }
                                    unreachable!()
                                })
                            })
                            .collect_vec();
                        for task in pipe_tasks {
                            let pipe = task.await;
                            log::debug!(
                                "add later pipe {} / {}",
                                pipe.protocol(),
                                pipe.peer_addr()
                            );
                            if let Some(multiplex) = weak_multiplex.upgrade() {
                                multiplex.add_pipe(pipe);
                            }
                        }
                    }
                }
            }));

            Ok(multiplex)
        }
    }
}

async fn connect_once(
    ctx: TunnelCtx,
    desc: BridgeDescriptor,
    meta: &str,
) -> anyhow::Result<Box<dyn Pipe>> {
    if let EndpointSource::Binder(params) = &ctx.endpoint {
        if params.use_bridges && desc.is_direct {
            anyhow::bail!("skipping direct connection")
        }
    }
    match desc.protocol.as_str() {
        "sosistab2-obfsudp" => {
            log::debug!("trying to connect to {}", desc.endpoint);
            (ctx.status_callback)(TunnelStatus::PreConnect {
                addr: desc.endpoint,
                protocol: "sosistab2-obfsudp".into(),
            });
            let keys: (ObfsUdpPublic, MuxPublic) =
                bincode::deserialize(&desc.sosistab_key).context("cannot decode keys")?;

            let connection = ObfsUdpPipe::connect(desc.endpoint, keys.0, meta)
                .timeout(Duration::from_secs(10))
                .await
                .context("pipe connection timeout")??;
            Ok(Box::new(connection))
        }
        "sosistab2-obfstls" => {
            let mut config = TlsConnector::builder();
            config
                .danger_accept_invalid_certs(true)
                .danger_accept_invalid_hostnames(true)
                .min_protocol_version(Some(Protocol::Tlsv12))
                .max_protocol_version(Some(Protocol::Tlsv12))
                .use_sni(true);
            let fake_domain = format!(
                "{}.{}{}.com",
                eff_wordlist::short::random_word(),
                eff_wordlist::large::random_word(),
                eff_wordlist::large::random_word()
            );
            let connection =
                ObfsTlsPipe::connect(desc.endpoint, &fake_domain, config, desc.sosistab_key, meta)
                    .timeout(Duration::from_secs(10))
                    .await
                    .context("pipe connection timeout")??;
            Ok(Box::new(connection))
        }
        other => {
            anyhow::bail!("unknown protocol {other}")
        }
    }
}
