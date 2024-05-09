use std::io::ErrorKind;
use std::sync::Arc;
use bitcoin::Txid;
use lightning::chain::chaininterface::YuvBroadcaster;
use lightning_block_sync::AsyncYuvSourceResult;
use lightning_block_sync::gossip::YuvTransactionSource;
use yuv_types::YuvTransaction;
use jsonrpsee::http_client::{HttpClient, HttpClientBuilder};
use lightning::{log_error, log_info};
use lightning::util::logger::Logger;
use yuv_rpc_api::transactions::{YuvTransactionsRpcClient, GetRawYuvTransactionResponse};
use yuv_rpc_api::transactions::EmulateYuvTransactionResponse;
use crate::disk::FilesystemLogger;

pub struct YuvClient {
    client: HttpClient,
    handle: tokio::runtime::Handle,
    logger: Arc<FilesystemLogger>,
}

impl YuvClient {
    pub(crate) fn new(
        yuv_node_url: String,
        handle: tokio::runtime::Handle,
        logger: Arc<FilesystemLogger>,
    ) -> Self {
        let http_client = HttpClientBuilder::new()
            .build(yuv_node_url)
            .expect("invalid yuv node url");

        Self {
            client: http_client,
            handle: handle.clone(),
            logger: logger.clone(),
        }
    }

    pub async fn get_list_raw_yuv_transactions(&self, txids: Vec<Txid>) -> Vec<YuvTransaction> {
        let logger = self.logger.clone();
        match self.client.get_list_raw_yuv_transactions(txids.clone()).await {
            Ok(yuv_txs) => yuv_txs,
            Err(err) => {
                log_error!(logger,
                   "Error, failed to getlistrawtransactions: {err}\nTx ids: {:?}", txids,
                );
                vec![]
            },
        }
    }

    pub async fn emulate_yuv_transaction(&self, yuv_tx: YuvTransaction) -> Option<String> {
        let logger = self.logger.clone();
        match self.client.emulate_yuv_transaction(yuv_tx.clone()).await {
            Ok(response) => {
                match response {
                    EmulateYuvTransactionResponse::Valid => None,
                    EmulateYuvTransactionResponse::Invalid { reason } => {
                        Some(reason)
                    },
                }
            },
            Err(err) => {
                log_error!(logger,
                    "Error, failed to emulateyuvtransaction: {err}\nTransaction: {:?}",
                    yuv_tx,
                );
                None
            },
        }
    }
}

impl YuvBroadcaster for YuvClient {
    fn broadcast_transactions_proofs(&self, yuv_tx: YuvTransaction) {
        let logger = self.logger.clone();
        let client = self.client.clone();
        self.handle.spawn(async move {
            match client.provide_yuv_proof(yuv_tx.clone()).await {
                Ok(_) => {
                    log_info!(logger, "Successfully broadcasted a YUV transaction")
                },
                Err(err) => {
                    log_error!(logger,
                       "Error, failed to provideyuvproof: {err}\nTransaction: {:?}", yuv_tx,
                    )
                },
            }
        });
    }

    fn emulate_yuv_transaction(&self, yuv_tx: YuvTransaction) -> Option<String> {
        tokio::task::block_in_place(move || {
            self.handle.block_on(async move {
                self.emulate_yuv_transaction(yuv_tx).await
            })
        })
    }
}


impl YuvTransactionSource for YuvClient {
    fn yuv_transaction_by_id<'a>(&'a self, txid: &'a Txid) -> AsyncYuvSourceResult<'a, GetRawYuvTransactionResponse> {
        let logger = self.logger.clone();
        let client = self.client.clone();

        Box::pin(async move {
            client.get_raw_yuv_transaction(*txid).await.map_err(|err| {
                log_error!(logger,
                    "Error, failed to getrawyuvtransaction: {err}\nTx id: {:?}", txid,
                );

                std::io::Error::new(ErrorKind::Other, "Failed to get raw yuv transaction").into()
            })
        })
    }
}
