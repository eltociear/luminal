use std::marker::PhantomData;

use half::f16;
use luminal::{
    nn::{embedding::Embedding, norm::RMSNorm},
    prelude::*,
    shape::symbolic::{BigExpression, Expression},
};

// Mistral 7B Config
pub const VOCAB_SIZE: usize = 32000;
pub const HIDDEN_DIM: usize = 4096;
pub const NUM_LAYERS: usize = 32;
pub const N_HEADS: usize = 32;
pub const N_KV_HEADS: usize = 8;
pub const MLP_DIM: usize = 14336;
pub const MLP_DIM_DOUBLE: usize = MLP_DIM * 2;

pub const N_ATTENTION_GROUPS: usize = N_HEADS / N_KV_HEADS;
pub const HEAD_DIM: usize = HIDDEN_DIM / N_HEADS;
pub const HEAD_DIM_OVER_2: usize = HEAD_DIM / 2;
pub const ATTN_PROJ_DIM: usize = HEAD_DIM * N_KV_HEADS;

pub type KVCache<Batch, Seq> = (
    GraphTensor<(Batch, Const<N_KV_HEADS>, Seq, Const<HEAD_DIM>)>,
    GraphTensor<(Batch, Const<N_KV_HEADS>, Seq, Const<HEAD_DIM>)>,
);

pub struct Mlp<const I: usize, const H: usize, const HI: usize> {
    pub gate_up_proj: GraphTensor<(Const<H>, Const<HI>)>,
    pub down_proj: GraphTensor<(Const<I>, Const<H>)>,
}

impl<Batch: Dimension, const I: usize, const H: usize, const HI: usize>
    Module<GraphTensor<(Batch, Const<H>)>> for Mlp<I, H, HI>
{
    type Output = GraphTensor<(Batch, Const<H>)>;

    fn forward(&self, input: GraphTensor<(Batch, Const<H>)>) -> Self::Output {
        let matmulled = input.matmul(self.gate_up_proj);
        let gate = matmulled
            .slice((.., ..Expression::from(I)))
            .realize::<(Batch, Const<I>)>()
            .swish();
        let up = matmulled
            .slice((.., Expression::from(I)..))
            .realize::<(Batch, Const<I>)>()
            * gate;
        up.matmul(self.down_proj)
    }
}

impl<Batch: Dimension, Seq: Dimension, const I: usize, const H: usize, const HI: usize>
    Module<GraphTensor<(Batch, Seq, Const<H>)>> for Mlp<I, H, HI>
{
    type Output = GraphTensor<(Batch, Seq, Const<H>)>;

    fn forward(&self, input: GraphTensor<(Batch, Seq, Const<H>)>) -> Self::Output {
        let matmulled = input.matmul(self.gate_up_proj);
        let gate = matmulled
            .slice((.., .., ..Expression::from(I)))
            .realize::<(Batch, Seq, Const<I>)>()
            .swish();
        let up = matmulled
            .slice((.., .., Expression::from(I)..))
            .realize::<(Batch, Seq, Const<I>)>()
            * gate;
        up.matmul(self.down_proj)
    }
}

impl<const I: usize, const H: usize, const HI: usize> InitModule for Mlp<I, H, HI> {
    fn initialize(cx: &mut Graph) -> Self {
        Self {
            gate_up_proj: cx.named_tensor("Gate-Up Weight"),
            down_proj: cx.named_tensor("Down Weight"),
        }
    }
}

impl<const I: usize, const H: usize, const HI: usize> SerializeModule for Mlp<I, H, HI> {
    fn serialize(&self, s: &mut Serializer) {
        s.tensor("gate_up_proj/weight", self.gate_up_proj);
        s.tensor("down_proj/weight", self.down_proj);
    }
}

pub struct RotaryEmbedding<const HEAD_DIM: usize, const HEAD_DIM_OVER_2: usize> {
    pub inv_freq: GraphTensor<R1<HEAD_DIM_OVER_2>>,
}

impl<
        Batch: Dimension,
        const NUM_HEADS: usize,
        Seq: Dimension,
        const HEAD_DIM: usize,
        const HEAD_DIM_OVER_2: usize,
    >
    Module<(
        GraphTensor<(Batch, Const<NUM_HEADS>, Seq, Const<HEAD_DIM>)>,
        BigExpression,
    )> for RotaryEmbedding<HEAD_DIM, HEAD_DIM_OVER_2>
{
    type Output = GraphTensor<(Batch, Const<NUM_HEADS>, Seq, Const<HEAD_DIM>)>;

    fn forward(
        &self,
        (inp, prev_seq): (
            GraphTensor<(Batch, Const<NUM_HEADS>, Seq, Const<HEAD_DIM>)>,
            BigExpression,
        ),
    ) -> Self::Output {
        let (sin, cos) = self.get_sincos::<NUM_HEADS, Seq>(prev_seq);
        (Self::rotate_half(inp) * sin.expand()) + (inp * cos.expand())
    }
}

impl<const HEAD_DIM: usize, const HEAD_DIM_OVER_2: usize>
    RotaryEmbedding<HEAD_DIM, HEAD_DIM_OVER_2>
{
    #[allow(clippy::type_complexity)]
    fn get_sincos<const NUM_HEADS: usize, Seq: Dimension>(
        &self,
        prev_seq: BigExpression,
    ) -> (
        GraphTensor<(Seq, Const<HEAD_DIM>)>,
        GraphTensor<(Seq, Const<HEAD_DIM>)>,
    ) {
        let t = self.inv_freq.graph().arange::<Seq>()
            + self.inv_freq.graph().constant_expr(prev_seq).expand();
        let freqs = t.expand::<(Seq, Const<1>), _>().matmul(
            self.inv_freq
                .expand::<(Const<1>, Const<HEAD_DIM_OVER_2>), _>(),
        );
        let emb = freqs.concat_along::<(Seq, Const<HEAD_DIM>), Axis<1>, _>(freqs);
        (emb.sin().reshape(), emb.cos().reshape())
    }

    fn rotate_half<Batch: Dimension, NumHeads: Dimension, Seq: Dimension>(
        x: GraphTensor<(Batch, NumHeads, Seq, Const<HEAD_DIM>)>,
    ) -> GraphTensor<(Batch, NumHeads, Seq, Const<HEAD_DIM>)> {
        let x1 = x
            .slice((.., .., .., ..Expression::from(HEAD_DIM_OVER_2)))
            .contiguous();
        let x2 = x
            .slice((.., .., .., Expression::from(HEAD_DIM_OVER_2)..))
            .contiguous();
        (-x2).concat_along::<(Batch, NumHeads, Seq, Const<HEAD_DIM>), Axis<3>, _>(x1)
    }
}

impl<const HEAD_DIM: usize, const HEAD_DIM_OVER_2: usize> InitModule
    for RotaryEmbedding<HEAD_DIM, HEAD_DIM_OVER_2>
{
    fn initialize(cx: &mut Graph) -> Self {
        fn compute_inv_freq(head_dim: usize) -> Vec<f32> {
            let theta = 1000000.0;
            let mut rope_graph = Graph::new();
            let frequencies = (rope_graph.arange::<Dyn<'h'>>() * 2.0) / (head_dim as f32);
            let frequencies = frequencies.inv_pow(theta).recip().retrieve();

            rope_graph.set_dyn_dim('h', head_dim / 2);
            rope_graph.execute();

            frequencies.data()
        }
        Self {
            inv_freq: cx
                .named_tensor("Inv Freq")
                .set(compute_inv_freq(HEAD_DIM))
                .keep(),
        }
    }
}

pub struct SelfAttention {
    pub q_proj: GraphTensor<R2<HIDDEN_DIM, HIDDEN_DIM>>,
    pub k_proj: GraphTensor<R2<HIDDEN_DIM, ATTN_PROJ_DIM>>,
    pub v_proj: GraphTensor<R2<HIDDEN_DIM, ATTN_PROJ_DIM>>,
    pub o_proj: GraphTensor<R2<HIDDEN_DIM, HIDDEN_DIM>>,
    pub rotary_embeddings: RotaryEmbedding<HEAD_DIM, HEAD_DIM_OVER_2>,
}

impl<Batch: Dimension, CurSeq: Dimension, PrevSeq: Dimension, TotSeq: Dimension>
    Module<(
        GraphTensor<(Batch, CurSeq, Const<HIDDEN_DIM>)>,
        Option<KVCache<Batch, PrevSeq>>,
        PhantomData<TotSeq>,
    )> for SelfAttention
{
    type Output = (
        GraphTensor<(Batch, CurSeq, Const<HIDDEN_DIM>)>,
        KVCache<Batch, TotSeq>,
    );
    fn forward(
        &self,
        (x, cache, _): (
            GraphTensor<(Batch, CurSeq, Const<HIDDEN_DIM>)>,
            Option<KVCache<Batch, PrevSeq>>,
            PhantomData<TotSeq>,
        ),
    ) -> Self::Output {
        // Apply the Projections
        let query_states = x
            .matmul(self.q_proj)
            .reshape::<(Batch, CurSeq, Const<N_HEADS>, Const<HEAD_DIM>)>()
            .permute::<_, Axes4<0, 2, 1, 3>>();
        let key_states = x
            .matmul(self.k_proj)
            .reshape::<(Batch, CurSeq, Const<N_KV_HEADS>, Const<HEAD_DIM>)>()
            .permute::<_, Axes4<0, 2, 1, 3>>();
        let value_states = x
            .matmul(self.v_proj)
            .reshape::<(Batch, CurSeq, Const<N_KV_HEADS>, Const<HEAD_DIM>)>()
            .permute::<_, Axes4<0, 2, 1, 3>>();

        // Apply the Rotary Embeddings
        let query_states = self
            .rotary_embeddings
            .forward((query_states, PrevSeq::const_size().into()));
        let key_states = self
            .rotary_embeddings
            .forward((key_states, PrevSeq::const_size().into()));

        // Add KV cache
        let (key_states, value_states) = if let Some((k_cache, v_cache)) = cache {
            (
                k_cache.concat_along::<_, Axis<2>, _>(key_states),
                v_cache.concat_along::<_, Axis<2>, _>(value_states),
            )
        } else {
            (key_states.realize(), value_states.realize())
        };

        // Repeat the KV States for Grouped-Query Attention
        let repeated_key_states = key_states
            .expand::<(_, _, Const<N_ATTENTION_GROUPS>, _, _), Axis<2>>()
            .reshape::<(Batch, Const<N_HEADS>, TotSeq, Const<HEAD_DIM>)>()
            .permute::<_, Axes4<0, 1, 3, 2>>();

        let repeated_value_states = value_states
            .expand::<(_, _, Const<N_ATTENTION_GROUPS>, _, _), Axis<2>>()
            .reshape::<(Batch, Const<N_HEADS>, TotSeq, Const<HEAD_DIM>)>();

        let mut attention_weights = query_states.matmul(repeated_key_states);
        attention_weights = attention_weights * (HEAD_DIM as f32).sqrt().recip();
        // We only mask on a non-kv cache pass
        if cache.is_none() {
            let attention_mask = self.k_proj.graph().triu::<CurSeq, TotSeq>(1) * f16::MIN.to_f32();
            attention_weights += attention_mask.expand();
        }
        attention_weights = attention_weights.softmax::<3>();

        (
            attention_weights
                .matmul(repeated_value_states)
                .permute::<_, Axes4<0, 2, 1, 3>>()
                .reshape::<(Batch, CurSeq, Const<HIDDEN_DIM>)>()
                .matmul(self.o_proj),
            (key_states, value_states),
        )
    }
}

impl InitModule for SelfAttention {
    fn initialize(cx: &mut Graph) -> Self {
        Self {
            q_proj: cx.named_tensor("Q Proj"),
            k_proj: cx.named_tensor("K Proj"),
            v_proj: cx.named_tensor("V Proj"),
            o_proj: cx.named_tensor("O Proj"),
            rotary_embeddings: InitModule::initialize(cx),
        }
    }
}

impl SerializeModule for SelfAttention {
    fn serialize(&self, s: &mut Serializer) {
        s.tensor("q_proj/weight", self.q_proj);
        s.tensor("v_proj/weight", self.v_proj);
        s.tensor("k_proj/weight", self.k_proj);
        s.tensor("o_proj/weight", self.o_proj);
    }
}

pub struct TransformerBlock {
    pub attention: SelfAttention,
    pub attention_norm: RMSNorm<HIDDEN_DIM>,
    pub feed_forward: Mlp<MLP_DIM, HIDDEN_DIM, MLP_DIM_DOUBLE>,
    pub feed_forward_norm: RMSNorm<HIDDEN_DIM>,
}

impl<Batch: Dimension, CurSeq: Dimension, PrevSeq: Dimension, TotSeq: Dimension>
    Module<(
        GraphTensor<(Batch, CurSeq, Const<HIDDEN_DIM>)>,
        Option<KVCache<Batch, PrevSeq>>,
        PhantomData<TotSeq>,
    )> for TransformerBlock
{
    type Output = (
        GraphTensor<(Batch, CurSeq, Const<HIDDEN_DIM>)>,
        KVCache<Batch, TotSeq>,
    );
    fn forward(
        &self,
        (x, cache, _): (
            GraphTensor<(Batch, CurSeq, Const<HIDDEN_DIM>)>,
            Option<KVCache<Batch, PrevSeq>>,
            PhantomData<TotSeq>,
        ),
    ) -> Self::Output {
        // Attention
        let mut residual = x;
        let (mut x, cache) =
            self.attention
                .forward((self.attention_norm.forward(x), cache, PhantomData::<TotSeq>));

        // Residual Addition
        x += residual;

        // Feed Forward
        residual = x;
        x = self.feed_forward.forward(self.feed_forward_norm.forward(x));

        // Residual Addition
        (x + residual, cache)
    }
}

impl InitModule for TransformerBlock {
    fn initialize(cx: &mut Graph) -> Self {
        Self {
            attention: InitModule::initialize(cx),
            attention_norm: {
                let mut norm = RMSNorm::initialize(cx);
                norm.epsilon = 1e-5;
                norm
            },
            feed_forward: InitModule::initialize(cx),
            feed_forward_norm: {
                let mut norm = RMSNorm::initialize(cx);
                norm.epsilon = 1e-5;
                norm
            },
        }
    }
}

impl SerializeModule for TransformerBlock {
    fn serialize(&self, s: &mut Serializer) {
        s.module("self_attn", &self.attention);
        s.module("input_layernorm", &self.attention_norm);
        s.module("post_attention_layernorm", &self.feed_forward_norm);
        s.module("mlp", &self.feed_forward);
    }
}

pub struct MistralLM {
    // Token embeddings
    pub embedding: Embedding<VOCAB_SIZE, HIDDEN_DIM>,
    // Transformer layers
    pub layers: Vec<TransformerBlock>,
    // Final Norm layer
    pub norm: RMSNorm<HIDDEN_DIM>,
    // LM Head Layer
    pub lm_head: GraphTensor<R2<HIDDEN_DIM, VOCAB_SIZE>>,
}

impl<Batch: Dimension, CurSeq: Dimension, PrevSeq: Dimension, TotSeq: Dimension>
    Module<(
        GraphTensor<(Batch, CurSeq)>,
        Option<Vec<KVCache<Batch, PrevSeq>>>,
        PhantomData<TotSeq>,
    )> for MistralLM
{
    type Output = (
        GraphTensor<(Batch, CurSeq, Const<VOCAB_SIZE>)>,
        Vec<KVCache<Batch, TotSeq>>,
    );
    fn forward(
        &self,
        (input, cache, _): (
            GraphTensor<(Batch, CurSeq)>,
            Option<Vec<KVCache<Batch, PrevSeq>>>,
            PhantomData<TotSeq>,
        ),
    ) -> Self::Output {
        let mut hidden_states = self.embedding.forward(input);

        let mut new_caches = vec![];
        for (i, layer) in self.layers.iter().enumerate() {
            let (new_hidden_states, (k_cache, v_cache)) = layer.forward((
                hidden_states,
                cache.as_ref().map(|c| c[i]),
                PhantomData::<TotSeq>,
            ));
            hidden_states = new_hidden_states;
            new_caches.push((k_cache.contiguous(), v_cache.contiguous()));
        }
        hidden_states = self.norm.forward(hidden_states);

        (hidden_states.matmul(self.lm_head), new_caches)
    }
}

impl InitModule for MistralLM {
    fn initialize(cx: &mut Graph) -> Self {
        Self {
            embedding: InitModule::initialize(cx),
            norm: InitModule::initialize(cx),
            lm_head: cx.named_tensor("LM Head"),
            layers: (0..NUM_LAYERS)
                .map(|_| InitModule::initialize(cx))
                .collect(),
        }
    }
}

impl SerializeModule for MistralLM {
    fn serialize(&self, s: &mut Serializer) {
        s.module("model/embed_tokens", &self.embedding);
        s.module("model/norm", &self.norm);
        s.tensor("lm_head/weight", self.lm_head);
        for (i, layer) in self.layers.iter().enumerate() {
            s.module(&format!("model/layers/{i}"), layer);
        }
    }
}
