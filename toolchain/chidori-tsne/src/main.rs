use tch::{Device, Tensor, Kind};
use std::error::Error;

fn hbeta(d: &Tensor, beta: f64) -> Result<(Tensor, Tensor), Box<dyn Error>> {
    let p = (-d * beta).exp();
    let sum_p = p.sum(Kind::Float);
    let h = sum_p.log() + (d * p).sum(Kind::Float) * beta / sum_p;
    let p = p / sum_p;
    Ok((h, p))
}

fn x2p(x: &Tensor, tol: f64, perplexity: f64) -> Result<Tensor, Box<dyn Error>> {
    let (n, _) = x.size2()?;
    let sum_x = x.pow(2).sum_dim_intlist(&[1], false, Kind::Float);
    let d = (&sum_x + &sum_x.t()) - 2. * x.matmul(&x.t());

    let mut p = Tensor::zeros(&[n, n], (Kind::Float, Device::Mps));
    let mut beta = Tensor::ones(&[n, 1], (Kind::Float, Device::Mps));
    let log_u = perplexity.log();

    for i in 0..n {
        // Compute the Gaussian kernel and entropy for the current precision
        let mut beta_min = None;
        let mut beta_max = None;
        let di = d.i(i).select(0, &Tensor::cat(&[d.i((0..i, i)), d.i((i+1..n, i))], 0));

        let (mut h, mut this_p) = hbeta(&di, beta.double_value(&[i, 0])?)?;

        // Evaluate whether the perplexity is within tolerance
        let mut h_diff = &h - log_u;
        let mut tries = 0;

        while h_diff.abs() > tol && tries < 50 {
            if h_diff > 0. {
                beta_min = Some(beta.double_value(&[i, 0])?);
                if beta_max.is_none() {
                    beta.i(i, 0).mul_assign(2.);
                } else {
                    beta.i(i, 0).copy_(&((beta.double_value(&[i, 0])? + beta_max.unwrap()) / 2.));
                }
            } else {
                beta_max = Some(beta.double_value(&[i, 0])?);
                if beta_min.is_none() {
                    beta.i(i, 0).div_assign(2.);
                } else {
                    beta.i(i, 0).copy_(&((beta.double_value(&[i, 0])? + beta_min.unwrap()) / 2.));
                }
            }

            // Recompute the values
            let (new_h, new_this_p) = hbeta(&di, beta.double_value(&[i, 0])?)?;
            h = new_h;
            this_p = new_this_p;
            h_diff = &h - log_u;
            tries += 1;
        }

        // Set the final row of P
        p.i(i).slice(0, 0, i, 1).copy_(&this_p.slice(0, 0, i, 1));
        p.i(i).slice(0, i+1, n, 1).copy_(&this_p.slice(0, i, n-1, 1));
    }

    Ok(p)
}

fn pca(x: &Tensor, no_dims: i64) -> Result<Tensor, Box<dyn Error>> {
    let (n, d) = x.size2()?;
    let x = x - x.mean_dim(&[0], true, Kind::Float);

    let (l, m) = x.t().matmul(&x).symeig(true, false)?;
    let y = x.matmul(&m.slice(1, 0, no_dims, 1));
    Ok(y)
}

fn tsne(x: &Tensor, no_dims: i64, perplexity: f64) -> Result<Tensor, Box<dyn Error>> {
    let initial_dims = 50;
    let x = pca(x, initial_dims)?;
    let (n, _) = x.size2()?;

    let mut y = Tensor::randn(&[n, no_dims], (Kind::Float, Device::Mps));
    let mut dy = Tensor::zeros(&[n, no_dims], (Kind::Float, Device::Mps));
    let mut iy = Tensor::zeros(&[n, no_dims], (Kind::Float, Device::Mps));
    let mut gains = Tensor::ones(&[n, no_dims], (Kind::Float, Device::Mps));

    // Compute P-values
    let mut p = x2p(x, 1e-5, perplexity)?;
    p = &p + &p.t();
    p /= p.sum(Kind::Float);
    p *= 4.;
    p = p.max1(&Tensor::from(1e-21f64));

    // Run iterations
    for iter in 0..1000 {
        // Compute pairwise affinities
        let sum_y = y.pow(2).sum_dim_intlist(&[1], true, Kind::Float);
        let num = -2. * y.matmul(&y.t());
        let num = 1. / (1. + &sum_y + &sum_y.t() + &num);
        num.diag_mut().zero_();
        let q = &num / num.sum(Kind::Float);
        let q = q.max1(&Tensor::from(1e-12f64));

        // Compute gradient
        let pq = &p - &q;
        for i in 0..n {
            dy.i(i).copy_(&(((&pq.i((.., i)) * &num.i((.., i))).repeat(&[no_dims]) * (&y - &y.i(i))).sum_dim_intlist(&[0], false, Kind::Float)));
        }

        // Perform the update
        let momentum = if iter < 20 { 0.5 } else { 0.8 };
        gains = (&gains + 0.2) * (dy.sign() != iy.sign()).to_kind(Kind::Float) +
            (&gains * 0.8) * (dy.sign() == iy.sign()).to_kind(Kind::Float);
        gains = gains.max1(&Tensor::from(0.01f64));
        iy = &iy * momentum - &(&gains * &dy) * 500.;
        y += &iy;
        y -= y.mean_dim(&[0], true, Kind::Float);

        // Compute current value of cost function
        if (iter + 1) % 10 == 0 {
            let c = (&p * (&p / &q).log()).sum(Kind::Float);
            println!("Iteration {}: error is {}", iter + 1, c);
        }

        // Stop lying about P-values
        if iter == 100 {
            p /= 4.;
        }
    }

    Ok(y)
}

fn main() -> Result<(), Box<dyn Error>> {
    // Load data (you'll need to implement this part)
    let x = Tensor::randn(&[2500, 784], (Kind::Float, Device::Mps));

    let y = tsne(&x, 2, 30.0)?;

    // Plotting (you'll need to implement this part, possibly using a Rust plotting library)

    Ok(())
}