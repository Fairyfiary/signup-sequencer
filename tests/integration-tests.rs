use ethers::{
    abi::Address,
    core::abi::Abi,
    prelude::{
        Bytes, ContractFactory, Http, LocalWallet, NonceManagerMiddleware, Provider, Signer,
        SignerMiddleware,
    },
    types::H256,
    utils::{Ganache, GanacheInstance},
};
use eyre::{bail, Result as EyreResult};
use hex_literal::hex;
use hyper::{client::HttpConnector, Body, Client, Request};
use semaphore::poseidon_tree::PoseidonTree;
use serde::{Deserialize, Serialize};
use serde_json::json;
use signup_sequencer::{
    app::{App, Hash, InclusionProofResponse},
    server, Options,
};
use std::{
    fs::File,
    io::BufReader,
    net::{IpAddr, Ipv4Addr, SocketAddr, TcpListener},
    str::FromStr,
    sync::Arc,
    time::Duration,
};
use structopt::StructOpt;
use tempfile::NamedTempFile;
use tokio::{spawn, sync::broadcast};
use url::{Host, Url};

const TEST_LEAFS: &[&str] = &[
    "0000000000000000000000000000000000000000000000000000000000000001",
    "0000000000000000000000000000000000000000000000000000000000000002",
    "0000000000000000000000000000000000000000000000000000000000000003",
];

const GANACHE_DEFAULT_WALLET_KEY: H256 = H256(hex!(
    "1ce6a4cc4c9941a4781349f988e129accdc35a55bb3d5b1a7b342bc2171db484"
));

#[tokio::test]
async fn insert_identity_and_proofs() {
    let mut options = Options::from_iter_safe(&[""]).expect("Failed to create options");
    options.server.server = Url::parse("http://127.0.0.1:0/").expect("Failed to parse URL");

    let temp_commitments_file = NamedTempFile::new().expect("Failed to create named temp file");
    options.app.storage_file = temp_commitments_file.path().to_path_buf();

    let (shutdown, _) = broadcast::channel(1);

    let (ganache, semaphore_address) = spawn_mock_chain()
        .await
        .expect("Failed to spawn ganache chain");

    options.app.ethereum.eip1559 = false;
    options.app.ethereum.ethereum_provider =
        Url::parse(&ganache.endpoint()).expect("Failed to parse ganache endpoint");
    options.app.ethereum.semaphore_address = semaphore_address;
    options.app.ethereum.signing_key = GANACHE_DEFAULT_WALLET_KEY;

    let local_addr = spawn_app(options.clone(), shutdown.clone())
        .await
        .expect("Failed to spawn app.");

    let uri = "http://".to_owned() + &local_addr.to_string();
    let mut ref_tree = PoseidonTree::new(options.app.tree_depth, options.app.initial_leaf);
    let client = Client::new();

    test_inclusion_proof(
        &uri,
        &client,
        0,
        &mut ref_tree,
        &options.app.initial_leaf,
        true,
    )
    .await;
    test_inclusion_proof(
        &uri,
        &client,
        1,
        &mut ref_tree,
        &options.app.initial_leaf,
        true,
    )
    .await;
    test_insert_identity(&uri, &client, TEST_LEAFS[0], 0).await;
    test_inclusion_proof(
        &uri,
        &client,
        0,
        &mut ref_tree,
        &Hash::from_str(TEST_LEAFS[0]).expect("Failed to parse Hash from test leaf 0"),
        false,
    )
    .await;
    test_insert_identity(&uri, &client, TEST_LEAFS[1], 1).await;
    test_inclusion_proof(
        &uri,
        &client,
        1,
        &mut ref_tree,
        &Hash::from_str(TEST_LEAFS[1]).expect("Failed to parse Hash from test leaf 1"),
        false,
    )
    .await;
    test_inclusion_proof(
        &uri,
        &client,
        2,
        &mut ref_tree,
        &options.app.initial_leaf,
        true,
    )
    .await;

    // Shutdown app and spawn new one from file
    let _ = shutdown.send(()).expect("Failed to send shutdown signal");

    let local_addr = spawn_app(options.clone(), shutdown.clone())
        .await
        .expect("Failed to spawn app.");
    let uri = "http://".to_owned() + &local_addr.to_string();

    test_inclusion_proof(
        &uri,
        &client,
        0,
        &mut ref_tree,
        &Hash::from_str(TEST_LEAFS[0]).expect("Failed to parse Hash from test leaf 0"),
        false,
    )
    .await;
    test_inclusion_proof(
        &uri,
        &client,
        1,
        &mut ref_tree,
        &Hash::from_str(TEST_LEAFS[1]).expect("Failed to parse Hash from test leaf 1"),
        false,
    )
    .await;

    temp_commitments_file
        .close()
        .expect("Failed to close temp file");
}

async fn test_inclusion_proof(
    uri: &str,
    client: &Client<HttpConnector>,
    leaf_index: usize,
    ref_tree: &mut PoseidonTree,
    leaf: &Hash,
    expect_failure: bool,
) {
    let body = construct_inclusion_proof_body(TEST_LEAFS[leaf_index]);
    let req = Request::builder()
        .method("POST")
        .uri(uri.to_owned() + "/inclusionProof")
        .header("Content-Type", "application/json")
        .body(body)
        .expect("Failed to create inclusion proof hyper::Body");

    let mut response = client
        .request(req)
        .await
        .expect("Failed to execute request.");
    if expect_failure {
        assert!(!response.status().is_success());
        return;
    } else {
        assert!(response.status().is_success());
    }

    let bytes = hyper::body::to_bytes(response.body_mut())
        .await
        .expect("Failed to convert response body to bytes");
    let result = String::from_utf8(bytes.into_iter().collect())
        .expect("Could not parse response bytes to utf-8");

    ref_tree.set(leaf_index, *leaf);
    let proof = ref_tree.proof(leaf_index).expect("Ref tree malfunctioning");
    let inclusion_proof = InclusionProofResponse {
        root: ref_tree.root(),
        proof,
    };

    let serialized_proof =
        serde_json::to_string_pretty(&inclusion_proof).expect("Proof serialization failed");

    assert_eq!(result, serialized_proof);
}

async fn test_insert_identity(
    uri: &str,
    client: &Client<HttpConnector>,
    identity_commitment: &str,
    identity_index: usize,
) {
    let body = construct_insert_identity_body(identity_commitment);
    let req = Request::builder()
        .method("POST")
        .uri(uri.to_owned() + "/insertIdentity")
        .header("Content-Type", "application/json")
        .body(body)
        .expect("Failed to create insert identity hyper::Body");

    let mut response = client
        .request(req)
        .await
        .expect("Failed to execute request.");
    assert!(response.status().is_success());

    let bytes = hyper::body::to_bytes(response.body_mut())
        .await
        .expect("Failed to convert response body to bytes");
    let result = String::from_utf8(bytes.into_iter().collect())
        .expect("Could not parse response bytes to utf-8");

    let expected = InsertIdentityResponse { identity_index };
    let expected = serde_json::to_string_pretty(&expected).expect("Index serialization failed");

    assert_eq!(result, expected);
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
struct InsertIdentityResponse {
    identity_index: usize,
}

fn construct_inclusion_proof_body(identity_commitment: &str) -> Body {
    Body::from(
        json!({
            "groupId": 0,
            "identityCommitment": identity_commitment,
        })
        .to_string(),
    )
}

fn construct_insert_identity_body(identity_commitment: &str) -> Body {
    Body::from(
        json!({
            "groupId": 0,
            "identityCommitment": identity_commitment,

        })
        .to_string(),
    )
}

async fn spawn_app(options: Options, shutdown: broadcast::Sender<()>) -> EyreResult<SocketAddr> {
    let app = Arc::new(App::new(options.app).await.expect("Failed to create App"));

    let ip: IpAddr = match options.server.server.host() {
        Some(Host::Ipv4(ip)) => ip.into(),
        Some(Host::Ipv6(ip)) => ip.into(),
        Some(_) => bail!("Cannot bind {}", options.server.server),
        None => Ipv4Addr::LOCALHOST.into(),
    };
    let port = options.server.server.port().unwrap_or(9998);
    let addr = SocketAddr::new(ip, port);
    let listener = TcpListener::bind(&addr).expect("Failed to bind random port");
    let local_addr = listener.local_addr()?;

    spawn({
        async move {
            server::bind_from_listener(app, listener, shutdown)
                .await
                .expect("Failed to bind address");
        }
    });

    Ok(local_addr)
}

#[derive(Deserialize, Serialize, Debug)]
struct CompiledContract {
    abi:      Abi,
    bytecode: String,
}

fn deserialize_to_bytes(input: String) -> EyreResult<Bytes> {
    if input.len() >= 2 && &input[0..2] == "0x" {
        let bytes: Vec<u8> = hex::decode(&input[2..])?;
        Ok(bytes.into())
    } else {
        bail!("Expected 0x prefix")
    }
}

async fn spawn_mock_chain() -> EyreResult<(GanacheInstance, Address)> {
    let ganache = Ganache::new().block_time(2u64).mnemonic("test").spawn();

    let provider = Provider::<Http>::try_from(ganache.endpoint())
        .expect("Failed to initialize ganache endpoint")
        .interval(Duration::from_millis(500u64));

    let wallet: LocalWallet = ganache.keys()[0].clone().into();

    // connect the wallet to the provider
    let client = SignerMiddleware::new(provider, wallet.clone());
    let client = NonceManagerMiddleware::new(client, wallet.address());
    let client = std::sync::Arc::new(client);

    let poseidon_t3_json =
        File::open("./sol/PoseidonT3.json").expect("Failed to read PoseidonT3.json");
    let poseidon_t3_json: CompiledContract =
        serde_json::from_reader(BufReader::new(poseidon_t3_json))
            .expect("Could not parse compiled PoseidonT3 contract");
    let poseidon_t3_bytecode = deserialize_to_bytes(poseidon_t3_json.bytecode)?;

    let poseidon_t3_factory =
        ContractFactory::new(poseidon_t3_json.abi, poseidon_t3_bytecode, client.clone());
    let poseidon_t3_contract = poseidon_t3_factory
        .deploy(())?
        .legacy()
        .confirmations(0usize)
        .send()
        .await?;

    let poseidon_t6_json =
        File::open("./sol/PoseidonT6.json").expect("Failed to read PoseidonT6.json");
    let poseidon_t6_json: CompiledContract =
        serde_json::from_reader(BufReader::new(poseidon_t6_json))
            .expect("Could not parse compiled PoseidonT6 contract");
    let poseidon_t6_bytecode = deserialize_to_bytes(poseidon_t6_json.bytecode)?;

    let poseidon_t6_factory =
        ContractFactory::new(poseidon_t6_json.abi, poseidon_t6_bytecode, client.clone());
    let poseidon_t6_contract = poseidon_t6_factory
        .deploy(())?
        .legacy()
        .confirmations(0usize)
        .send()
        .await?;

    let semaphore_json =
        File::open("./sol/Semaphore.json").expect("Compiled contract doesn't exist");
    let semaphore_json: CompiledContract =
        serde_json::from_reader(BufReader::new(semaphore_json)).expect("Could not read contract");

    let semaphore_bytecode = semaphore_json.bytecode.replace(
        "__$86e0c54df1042e28bf2dd20ccf184dc428$__",
        &format!("{:?}", poseidon_t3_contract.address()).replace("0x", ""),
    );
    let semaphore_bytecode = semaphore_bytecode.replace(
        "__$223301b175b06473d424a7afc957d4e96c$__",
        &format!("{:?}", poseidon_t6_contract.address()).replace("0x", ""),
    );
    let semaphore_bytecode = deserialize_to_bytes(semaphore_bytecode)?;

    // create a factory which will be used to deploy instances of the contract
    let semaphore_factory =
        ContractFactory::new(semaphore_json.abi, semaphore_bytecode, client.clone());

    let semaphore_contract = semaphore_factory
        .deploy((4_u64, 123_u64))?
        .legacy()
        .confirmations(0usize)
        .send()
        .await?;

    Ok((ganache, semaphore_contract.address()))
}
