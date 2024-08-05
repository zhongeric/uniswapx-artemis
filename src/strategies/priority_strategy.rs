use super::{
    shared::UniswapXStrategy,
    types::{Config, OrderStatus},
};
use crate::collectors::{
    block_collector::NewBlock,
    uniswapx_order_collector::UniswapXOrder,
    uniswapx_route_collector::{OrderBatchData, OrderData, PriorityOrderData, RoutedOrder},
};
use alloy_primitives::Uint;
use anyhow::Result;
use artemis_core::executors::mempool_executor::{GasBidInfo, SubmitTxToMempool};
use artemis_core::types::Strategy;
use async_trait::async_trait;
use bindings_uniswapx::shared_types::SignedOrder;
use ethers::{
    providers::Middleware,
    types::{Address, Bytes, Filter, U256},
    utils::hex,
};
use std::collections::HashMap;
use std::error::Error;
use std::str::FromStr;
use std::sync::Arc;
use tokio::sync::mpsc::{Receiver, Sender};
use tracing::{error, info};
use uniswapx_rs::order::{OrderResolution, PriorityOrder};

use super::types::{Action, Event};

const DONE_EXPIRY: u64 = 300;
// Base addresses
const REACTOR_ADDRESS: &str = "0x000000001Ec5656dcdB24D90DFa42742738De729";
pub const WETH_ADDRESS: &str = "0x4200000000000000000000000000000000000006";

#[derive(Debug)]
#[allow(dead_code)]
pub struct UniswapXPriorityFill<M> {
    /// Ethers client.
    client: Arc<M>,
    /// Amount of profits to bid in gas
    bid_percentage: u64,
    last_block_number: u64,
    last_block_timestamp: u64,
    // map of open order hashes to order data
    open_orders: HashMap<String, PriorityOrderData>,
    // map of done order hashes to time at which we can safely prune them
    done_orders: HashMap<String, u64>,
    batch_sender: Sender<Vec<OrderBatchData>>,
    route_receiver: Receiver<RoutedOrder>,
}

impl<M: Middleware + 'static> UniswapXPriorityFill<M> {
    pub fn new(
        client: Arc<M>,
        config: Config,
        sender: Sender<Vec<OrderBatchData>>,
        receiver: Receiver<RoutedOrder>,
    ) -> Self {
        info!("syncing state");

        Self {
            client,
            bid_percentage: config.bid_percentage,
            last_block_number: 0,
            last_block_timestamp: 0,
            open_orders: HashMap::new(),
            done_orders: HashMap::new(),
            batch_sender: sender,
            route_receiver: receiver,
        }
    }
}

#[async_trait]
impl<M: Middleware + 'static> Strategy<Event, Action> for UniswapXPriorityFill<M> {
    // In order to sync this strategy, we need to get the current bid for all Sudo pools.
    async fn sync_state(&mut self) -> Result<()> {
        info!("syncing state");

        Ok(())
    }

    // Process incoming events, seeing if we can arb new orders, and updating the internal state on new blocks.
    async fn process_event(&mut self, event: Event) -> Option<Action> {
        match event {
            Event::PriorityOrder(order) => self.process_order_event(*order).await,
            Event::NewBlock(block) => self.process_new_block_event(block).await,
            Event::UniswapXRoute(route) => self.process_new_route(*route).await,
            _ => None,
        }
    }
}

impl<M: Middleware + 'static> UniswapXStrategy<M> for UniswapXPriorityFill<M> {}

impl<M: Middleware + 'static> UniswapXPriorityFill<M> {
    fn decode_order(&self, encoded_order: &str) -> Result<PriorityOrder, Box<dyn Error>> {
        let encoded_order = if encoded_order.starts_with("0x") {
            &encoded_order[2..]
        } else {
            encoded_order
        };
        let order_hex = hex::decode(encoded_order)?;

        Ok(PriorityOrder::_decode(&order_hex, false)?)
    }

    async fn process_order_event(&mut self, event: UniswapXOrder) -> Option<Action> {
        if self.last_block_timestamp == 0 {
            return None;
        }

        let order = self
            .decode_order(&event.encoded_order)
            .map_err(|e| error!("failed to decode: {}", e))
            .ok()?;

        self.update_order_state(order, event.signature, event.order_hash);
        None
    }

    async fn process_new_route(&mut self, event: RoutedOrder) -> Option<Action> {
        if event
            .request
            .orders
            .iter()
            .any(|o| self.done_orders.contains_key(&o.hash()))
        {
            return None;
        }

        let OrderBatchData {
            // orders,
            orders,
            amount_out_required,
            ..
        } = &event.request;

        if let Some(profit) = self.get_profit_eth(&event) {
            info!(
                "Sending trade: num trades: {} routed quote: {}, batch needs: {}, profit: {} wei",
                orders.len(),
                event.route.quote,
                amount_out_required,
                profit
            );

            let signed_orders = self.get_signed_orders(orders.clone()).ok()?;
            return Some(Action::SubmitPublicTx(SubmitTxToMempool {
                tx: self
                    .build_fill(self.client.clone(), signed_orders, event)
                    .await
                    .ok()?,
                gas_bid_info: Some(GasBidInfo {
                    bid_percentage: self.bid_percentage,
                    total_profit: profit,
                }),
            }));
        }

        None
    }

    /// Process new block events, updating the internal state.
    async fn process_new_block_event(&mut self, event: NewBlock) -> Option<Action> {
        self.last_block_number = event.number.as_u64();
        self.last_block_timestamp = event.timestamp.as_u64();

        info!(
            "Processing block {} at {}, Order set sizes -- open: {}, done: {}",
            event.number,
            event.timestamp,
            self.open_orders.len(),
            self.done_orders.len()
        );
        self.handle_fills()
            .await
            .map_err(|e| error!("Error handling fills {}", e))
            .ok()?;
        self.update_open_orders();
        self.prune_done_orders();

        self.batch_sender
            .send(self.get_order_batches())
            .await
            .ok()?;

        None
    }

    /// encode orders into generic signed orders
    fn get_signed_orders(&self, orders: Vec<OrderData>) -> Result<Vec<SignedOrder>> {
        let mut signed_orders: Vec<SignedOrder> = Vec::new();
        for batch in orders.iter() {
            match batch {
                OrderData::PriorityOrderData(order) => {
                    signed_orders.push(SignedOrder {
                        order: Bytes::from(order.order._encode()),
                        sig: Bytes::from_str(&order.signature)?,
                    });
                }
                _ => {
                    return Err(anyhow::anyhow!("Invalid order type"));
                }
            }
        }
        Ok(signed_orders)
    }

    /// We do not batch orders because priority fee is applied on the transaction level
    fn get_order_batches(&self) -> Vec<OrderBatchData> {
        let mut order_batches: Vec<OrderBatchData> = Vec::new();

        // generate batches of size 1
        self.open_orders.iter().for_each(|(_, order_data)| {
            let amount_in = order_data.resolved.input.amount;
            let amount_out = order_data
                .resolved
                .outputs
                .iter()
                .fold(Uint::from(0), |sum, output| sum.wrapping_add(output.amount));

            order_batches.push(OrderBatchData {
                orders: vec![OrderData::PriorityOrderData(order_data.clone())],
                amount_in,
                amount_out_required: amount_out,
                token_in: order_data.resolved.input.token.clone(),
                token_out: order_data.resolved.outputs[0].token.clone(),
            });
        });
        order_batches
    }

    async fn handle_fills(&mut self) -> Result<()> {
        let reactor_address = REACTOR_ADDRESS.parse::<Address>().unwrap();
        let filter = Filter::new()
            .select(self.last_block_number)
            .address(reactor_address)
            .event("Fill(bytes32,address,address,uint256)");

        // early return on error
        let logs = self.client.get_logs(&filter).await?;
        for log in logs {
            let order_hash = format!("0x{:x}", log.topics[1]);
            // remove from open
            info!("Removing filled order {}", order_hash);
            self.open_orders.remove(&order_hash);
            // add to done
            self.done_orders.insert(
                order_hash.to_string(),
                self.current_timestamp()? + DONE_EXPIRY,
            );
        }

        Ok(())
    }

    /// We still calculate profit in terms of ETH for priority fee orders
    /// Rationale:
    ///     - we have to bid at least the base fee
    ///     - the priority fee set for the transaction is essentially total_profit_eth - base_fee
    ///     - at 100% bid_percentage, our priority fee is total_profit_eth and thus gives the maximum amount to the user
    fn get_profit_eth(&self, RoutedOrder { request, route }: &RoutedOrder) -> Option<U256> {
        let quote = U256::from_str_radix(&route.quote, 10).ok()?;
        let amount_out_required =
            U256::from_str_radix(&request.amount_out_required.to_string(), 10).ok()?;
        if quote.le(&amount_out_required) {
            return None;
        }
        let profit_quote = quote.saturating_sub(amount_out_required);

        if request.token_out.to_lowercase() == WETH_ADDRESS.to_lowercase() {
            return Some(profit_quote);
        }

        let gas_use_eth = U256::from_str_radix(&route.gas_use_estimate, 10)
            .ok()?
            .saturating_mul(U256::from_str_radix(&route.gas_price_wei, 10).ok()?);
        profit_quote
            .saturating_mul(gas_use_eth)
            .checked_div(U256::from_str_radix(&route.gas_use_estimate_quote, 10).ok()?)
    }

    fn update_order_state(&mut self, order: PriorityOrder, signature: String, order_hash: String) {
        let resolved = order.resolve(Uint::from(0));
        let order_status: OrderStatus = match resolved {
            OrderResolution::Expired => OrderStatus::Done,
            OrderResolution::Invalid => OrderStatus::Done,
            OrderResolution::Resolved(resolved_order) => OrderStatus::Open(resolved_order),
        };

        match order_status {
            OrderStatus::Done => {
                self.mark_as_done(&order_hash);
            }
            OrderStatus::Open(resolved_order) => {
                if self.done_orders.contains_key(&order_hash) {
                    info!("Order already done, skipping: {}", order_hash);
                    return;
                }
                if !self.open_orders.contains_key(&order_hash) {
                    info!("Adding new order {}", order_hash);
                }
                self.open_orders.insert(
                    order_hash.clone(),
                    PriorityOrderData {
                        order,
                        hash: order_hash,
                        signature,
                        resolved: resolved_order,
                    },
                );
            }
        }
    }

    fn prune_done_orders(&mut self) {
        let mut to_remove = Vec::new();
        for (order_hash, deadline) in self.done_orders.iter() {
            if *deadline < self.last_block_timestamp {
                to_remove.push(order_hash.clone());
            }
        }
        for order_hash in to_remove {
            self.done_orders.remove(&order_hash);
        }
    }

    fn update_open_orders(&mut self) {
        // TODO: this is nasty, plz cleanup
        let binding = self.open_orders.clone();
        let order_hashes: Vec<(&String, &PriorityOrderData)> = binding.iter().collect();
        for (order_hash, order_data) in order_hashes {
            self.update_order_state(
                order_data.order.clone(),
                order_data.signature.clone(),
                order_hash.clone().to_string(),
            );
        }
    }

    fn mark_as_done(&mut self, order: &str) {
        if self.open_orders.contains_key(order) {
            self.open_orders.remove(order);
        }
        if !self.done_orders.contains_key(order) {
            self.done_orders
                .insert(order.to_string(), self.last_block_timestamp + DONE_EXPIRY);
        }
    }
}