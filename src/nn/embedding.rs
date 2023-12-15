use rand::{thread_rng, Rng};

use crate::{op, prelude::*};

// This is a really shoddy way to do gathers TODO: Do something else

// Gather batch of batches from 2D embedding matrix
impl<S: Dimension, const DIM: usize> GraphTensor<(S, Const<DIM>)> {
    pub fn gather<S1: Dimension, S2: Dimension>(
        self,
        mut indexes: GraphTensor<(S1, S2)>,
    ) -> GraphTensor<(S1, S2, Const<DIM>)> {
        let res = self
            .graph()
            .add_op(op::Function(
                "Gather".to_string(),
                Box::new(|tensors| {
                    let indexes = tensors[0]
                        .0
                        .borrowed()
                        .data
                        .as_any()
                        .downcast_ref::<Vec<f32>>()
                        .unwrap();
                    let data = tensors[1]
                        .0
                        .borrowed()
                        .data
                        .as_any()
                        .downcast_ref::<Vec<f32>>()
                        .unwrap();
                    let mut res = vec![0.; indexes.len() * DIM];
                    let (a_ind, a_val, b_ind, b_val) = (
                        tensors[0].1.index_expression(),
                        tensors[0].1.valid_expression(),
                        tensors[1].1.index_expression(),
                        tensors[1].1.valid_expression(),
                    );
                    for i in 0..indexes.len() {
                        if a_val.exec_single_var(i) == 0 {
                            continue;
                        }
                        let start = indexes[a_ind.exec_single_var(i)] as usize * DIM;
                        for n in 0..DIM {
                            res[i * DIM + n] = if b_val.exec_single_var(start + n) != 0 {
                                data[b_ind.exec_single_var(start + n)]
                            } else {
                                0.0
                            };
                        }
                    }
                    vec![Tensor {
                        data: Box::new(res),
                    }]
                }),
            ))
            .input(indexes.id, 0, indexes.shape)
            .input(self.id, 0, self.shape) // Since indexes might have a 1 dimension we don't want getting changed, we feed it in as the first argument
            .finish();

        indexes.shape.expand(2, DIM.into());
        GraphTensor::from_id(res, indexes.shape.contiguous(), self.graph_ref)
    }
}

pub struct Embedding<const N: usize, const DIM: usize> {
    pub weight: GraphTensor<R2<N, DIM>>,
}

impl<const A: usize, const B: usize> InitModule for Embedding<A, B> {
    fn initialize(cx: &mut Graph) -> Self {
        let s = Self {
            weight: cx.named_tensor("Embedding Weight"),
        };
        // Init weight as uniform(-1, 1)
        let mut rng = thread_rng();
        s.weight.set(
            (0..(A * B))
                .map(|_| rng.gen_range(-1_f32..1_f32))
                .collect::<Vec<_>>(),
        );
        s
    }
}

impl<const A: usize, const B: usize> SerializeModule for Embedding<A, B> {
    fn serialize(&self, s: &mut crate::serialization::Serializer) {
        s.tensor("weight", self.weight);
    }
}

// Single
impl<S: Dimension, const N: usize, const DIM: usize> Module<GraphTensor<(S,)>>
    for Embedding<N, DIM>
{
    type Output = GraphTensor<(S, Const<DIM>)>;

    fn forward(&self, input: GraphTensor<(S,)>) -> Self::Output {
        <Self as Module<GraphTensor<(Const<1>, S)>>>::forward(self, input.expand()).max_reduce()
    }
}

// Batch
impl<B: Dimension, S: Dimension, const N: usize, const DIM: usize> Module<GraphTensor<(B, S)>>
    for Embedding<N, DIM>
{
    type Output = GraphTensor<(B, S, Const<DIM>)>;

    fn forward(&self, input: GraphTensor<(B, S)>) -> Self::Output {
        self.weight.gather(input)
    }
}

#[cfg(test)]
mod tests {
    use dfdx::{
        prelude::Module as DfdxModule,
        tensor::{Cpu, TensorFromVec},
    };

    use crate::{
        prelude::{Module, *},
        tests::assert_close,
    };

    use super::Embedding;
    use dfdx::nn::BuildOnDevice;

    #[test]
    fn test_embedding() {
        let mut cx = Graph::new();
        let batch = cx.tensor::<R2<2, 3>>();
        let a = cx.tensor::<R1<3>>();

        let model: Embedding<3, 4> = InitModule::initialize(&mut cx);
        model
            .weight
            .set(vec![1.1, 2., 3., 1., 2., 3., 14., 2., 33., 1., 2., 3.]);
        let b = model.forward(a);
        let batch_out = model.forward(batch);

        b.retrieve();
        a.retrieve();
        batch_out.retrieve();
        a.set(vec![1.0, 0.0, 1.0]);
        batch.set(vec![1.0, 0.0, 2.0, 1.0, 0.0, 1.0]);

        cx.execute();

        let d_dev = Cpu::default();
        let mut d_model = <dfdx::nn::modules::builders::Embedding<3, 4>>::build_on_device(&d_dev);
        d_model.weight = d_dev.tensor_from_vec(
            vec![1.1, 2., 3., 1., 2., 3., 14., 2., 33., 1., 2., 3.],
            (dfdx::shapes::Const::<3>, dfdx::shapes::Const::<4>),
        );
        let d_a = d_dev.tensor_from_vec(vec![1, 0, 1], (dfdx::shapes::Const::<3>,));
        let d_batch = d_dev.tensor_from_vec(
            vec![1, 0, 2, 1, 0, 1],
            (dfdx::shapes::Const::<2>, dfdx::shapes::Const::<3>),
        );

        let d_b = d_model.forward(d_a);
        let d_batch_out = d_model.forward(d_batch);

        assert_close(&b.data(), &d_b.as_vec());
        assert_close(&batch_out.data(), &d_batch_out.as_vec());
    }
}
